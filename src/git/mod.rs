pub mod command;
pub mod desktop;
pub mod git_service;
mod git_ui;
pub mod model;
pub mod native_client;
pub mod review;

#[cfg(test)]
mod test_git_process;
#[cfg(test)]
mod test_git_service;

use crate::git::command::GitRepository;
use crate::persistence;
use crate::remote::{RemoteAction, RemoteActionPayload, RemoteClientHandle};
use git_service::{GitBranch, GitDiffResult, GitLogEntry, GitStatusEntry, GitStatusResult};
use gpui::{
    anchored, deferred, div, prelude::*, px, Context, Corner, FocusHandle, IntoElement,
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
    pub ahead: u32,
    pub status_error: Option<String>,
    pub status_known: bool,
}

impl RepoEntry {
    fn apply_status(&mut self, status: &Result<GitStatusResult, String>) {
        self.status_known = true;
        match status {
            Ok(status) => {
                self.has_changes = !status.entries.is_empty();
                self.ahead = status.ahead;
                self.behind = status.behind;
                self.status_error = None;
            }
            Err(error) => self.status_error = Some(error.clone()),
        }
    }

    pub fn status_label(&self) -> String {
        if !self.status_known {
            return "Checking…".into();
        }
        if self.status_error.is_some() {
            return "Could not read status".into();
        }
        let mut parts = Vec::new();
        if self.has_changes {
            parts.push("Changes".to_string());
        }
        if self.ahead > 0 {
            parts.push(format!("↑ {}", self.ahead));
        }
        if self.behind > 0 {
            parts.push(format!("↓ {}", self.behind));
        }
        if parts.is_empty() {
            "Up to date locally".into()
        } else {
            parts.join(" · ")
        }
    }
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
    pub tokens: crate::ui::tokens::ThemeTokens,
    pub is_committing: bool,
    pub is_mutating: bool,
    pub commit_inputs: Option<(
        gpui::Entity<gpui_component::input::InputState>,
        gpui::Entity<gpui_component::input::InputState>,
    )>,
    input_subscriptions: Vec<gpui::Subscription>,
    repo_epoch: u64,
    status_epoch: u64,
    file_diff_epoch: u64,
    draft_epoch: u64,
    pub repos: Vec<RepoEntry>,
    repo_scan_epoch: u64,
    pub active_repo: usize,
    pub show_repo_dropdown: bool,
    focus: FocusHandle,
    pub active_view: GitView,
    pub status: Option<GitStatusResult>,
    pub selected_file: Option<String>,
    pub selected_file_staged: bool,
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
    Local(Vec<GitRepository>),
    Remote(RemoteClientHandle),
    Native(native_client::NativeGitClient),
}

#[derive(Clone)]
enum GitCommandClient {
    Remote(RemoteClientHandle),
    Native(native_client::NativeGitClient),
}
impl GitCommandClient {
    fn request(&self, action: RemoteAction) -> Result<crate::remote::RemoteActionResult, String> {
        match self {
            Self::Remote(client) => client.request(action),
            Self::Native(client) => client.request(action),
        }
    }
    fn has_control(&self) -> bool {
        match self {
            Self::Remote(client) => client
                .latest_snapshot()
                .map(|s| s.you_have_control)
                .unwrap_or(false),
            Self::Native(_) => true,
        }
    }
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
    pub fn new_local(
        repos: Vec<(String, String)>,
        repositories: Vec<GitRepository>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_backend(repos, GitBackend::Local(repositories), cx)
    }

    pub fn new_remote(
        repos: Vec<(String, String)>,
        client: RemoteClientHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_backend(repos, GitBackend::Remote(client), cx)
    }

    pub fn new_native(
        repos: Vec<(String, String)>,
        client: native_client::NativeGitClient,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        use gpui_component::input::{InputEvent, InputState};
        let mut view = Self::new_with_backend(repos, GitBackend::Native(client.clone()), cx);
        view.reload_repositories(cx);
        let summary = cx.new(|cx| InputState::new(window, cx).placeholder("Commit summary"));
        let description = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Description (optional)")
        });
        for (input, field) in [
            (&summary, GitField::CommitSummary),
            (&description, GitField::CommitDescription),
        ] {
            view.input_subscriptions
                .push(
                    cx.subscribe(input, move |this, input, event, cx| match event {
                        InputEvent::Change => {
                            let value = input.read(cx).value().to_string();
                            let current = match field {
                                GitField::CommitSummary => &this.commit_summary,
                                _ => &this.commit_description,
                            };
                            if current != &value {
                                this.draft_epoch += 1;
                                match field {
                                    GitField::CommitSummary => this.commit_summary = value,
                                    _ => this.commit_description = value,
                                }
                                cx.notify();
                            }
                        }
                        InputEvent::Focus => this.active_field = None,
                        _ => {}
                    }),
                );
        }
        view.commit_inputs = Some((summary, description));
        view
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
                ahead: 0,
                status_error: None,
                status_known: false,
            })
            .collect();
        let mut win = Self {
            backend,
            tokens: crate::ui::tokens::RuntimePreferencesSnapshot::default().tokens(),
            is_committing: false,
            is_mutating: false,
            commit_inputs: None,
            input_subscriptions: Vec::new(),
            repo_epoch: 0,
            status_epoch: 0,
            file_diff_epoch: 0,
            draft_epoch: 0,
            repos,
            repo_scan_epoch: 0,
            active_repo: 0,
            show_repo_dropdown: false,
            focus,
            active_view: GitView::Changes,
            status: None,
            selected_file: None,
            selected_file_staged: false,
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
        if matches!(win.backend, GitBackend::Local(_)) {
            win.load_persisted_token();
        }
        win.fetch_github_username(cx);
        if !matches!(win.backend, GitBackend::Native(_)) {
            win.refresh_status(cx);
        }
        win
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    pub fn repo_path(&self) -> &str {
        self.repos
            .get(self.active_repo)
            .map(|repo| repo.path.as_str())
            .unwrap_or("")
    }

    pub fn repo_label(&self) -> &str {
        self.repos
            .get(self.active_repo)
            .map(|repo| repo.label.as_str())
            .unwrap_or(if self.is_loading {
                "Loading repositories…"
            } else {
                "No repositories"
            })
    }

    fn remote_client(&self) -> Option<GitCommandClient> {
        match &self.backend {
            GitBackend::Local(_) => None,
            GitBackend::Remote(client) => Some(GitCommandClient::Remote(client.clone())),
            GitBackend::Native(client) => Some(GitCommandClient::Native(client.clone())),
        }
    }

    fn local_repository(&self) -> Option<GitRepository> {
        match &self.backend {
            GitBackend::Local(repositories) => repositories.get(self.active_repo).cloned(),
            GitBackend::Remote(_) | GitBackend::Native(_) => None,
        }
    }

    fn is_native(&self) -> bool {
        matches!(self.backend, GitBackend::Native(_))
    }

    fn is_remote(&self) -> bool {
        !matches!(self.backend, GitBackend::Local(_))
    }

    fn set_remote_auth_state(&mut self, has_token: bool, username: Option<String>) {
        self.github_token = has_token.then(|| "__remote_host__".to_string());
        self.github_username = username;
    }

    fn has_mutation_control(&self) -> bool {
        self.remote_client()
            .map(|client| client.has_control())
            .unwrap_or(true)
    }

    fn ensure_mutation_control(&mut self, cx: &mut Context<Self>) -> bool {
        if self.is_mutating || self.is_loading || self.repos.is_empty() {
            return false;
        }
        if matches!(self.backend, GitBackend::Local(_)) && !self.ensure_config_write_available(cx) {
            return false;
        }
        if self.has_mutation_control() {
            self.repo_scan_epoch += 1;
            return true;
        }
        self.operation_result = Some((
            false,
            "Take control before changing Git state on the remote host.".to_string(),
        ));
        cx.notify();
        false
    }

    fn ensure_config_write_available(&mut self, cx: &mut Context<Self>) -> bool {
        match persistence::active_config_write_availability() {
            persistence::ConfigWriteAvailability::Ready => true,
            persistence::ConfigWriteAvailability::Unavailable { diagnostic } => {
                self.operation_result = Some((
                    false,
                    format!(
                        "GitHub/configuration writes are unavailable until ConfigStore is repaired: {diagnostic}"
                    ),
                ));
                cx.notify();
                false
            }
        }
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
        if self.is_mutating || self.is_loading {
            return;
        }
        self.repo_epoch += 1;
        self.is_generating_message = false;
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

    pub fn reload_repositories(&mut self, cx: &mut Context<Self>) {
        if self.is_mutating {
            return;
        }
        let GitBackend::Native(client) = &self.backend else {
            self.refresh_all_repo_statuses(cx);
            self.refresh_status(cx);
            return;
        };
        let client = client.clone();
        let previous = self.repo_path().to_string();
        self.is_generating_message = false;
        self.is_loading = true;
        self.repo_epoch += 1;
        self.repo_scan_epoch += 1;
        let epoch = self.repo_epoch;
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.load_repositories() })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if this.repo_epoch != epoch {
                    return;
                }
                match result {
                    Ok((client, repos)) => {
                        this.backend = GitBackend::Native(client);
                        this.repos = repos
                            .into_iter()
                            .map(|(label, path)| RepoEntry {
                                label,
                                path,
                                has_changes: false,
                                behind: 0,
                                ahead: 0,
                                status_error: None,
                                status_known: false,
                            })
                            .collect();
                        this.active_repo = this
                            .repos
                            .iter()
                            .position(|repo| repo.path == previous)
                            .unwrap_or(0);
                        this.status = None;
                        this.file_diff = None;
                        if this.repo_path() != previous {
                            this.commit_summary.clear();
                            this.commit_description.clear();
                            this.draft_epoch += 1;
                        }
                        this.operation_result = None;
                        this.refresh_status(cx);
                        this.refresh_all_repo_statuses(cx);
                        if this.active_view == GitView::History && !this.repos.is_empty() {
                            this.load_history(cx);
                        }
                    }
                    Err(error) => {
                        this.is_loading = false;
                        this.operation_result = Some((false, error));
                    }
                }
                cx.notify();
            });
        });
    }

    pub fn fetch_all_repositories(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let Some(client) = self.remote_client() else {
            return;
        };
        let repos = self.repos.clone();
        self.is_mutating = true;
        self.operation_result = Some((true, "Fetching repositories…".into()));
        cx.notify();
        git_spawn!(cx, |this, cx| {
            let mut failures = Vec::new();
            for repo in repos {
                let client = client.clone();
                let path = repo.path.clone();
                let result = cx
                    .background_executor()
                    .spawn(
                        async move { client.request(RemoteAction::GitFetch { repo_path: path }) },
                    )
                    .await;
                match result {
                    Ok(result) if result.ok => {}
                    Ok(result) => failures.push(format!(
                        "{}: {}",
                        repo.label,
                        result.message.unwrap_or_else(|| "Fetch failed".into())
                    )),
                    Err(error) => failures.push(format!("{}: {error}", repo.label)),
                }
                // Closing the window cancels admission of subsequent fetches.
                if this
                    .update(&mut cx, |this, cx| {
                        this.operation_result = Some((true, format!("Fetched {}", repo.label)));
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                this.operation_result = Some((
                    failures.is_empty(),
                    if failures.is_empty() {
                        "All repositories fetched".into()
                    } else {
                        failures.join("; ")
                    },
                ));
                this.refresh_status(cx);
                this.refresh_all_repo_statuses(cx);
                cx.notify();
            });
        });
    }

    pub fn refresh_all_repo_statuses(&mut self, cx: &mut Context<Self>) {
        self.repo_scan_epoch += 1;
        let scan_epoch = self.repo_scan_epoch;
        let paths: Vec<(usize, String)> = self
            .repos
            .iter()
            .enumerate()
            .map(|(i, r)| (i, r.path.clone()))
            .collect();

        let remote_client = self.remote_client();
        let local_repositories = match &self.backend {
            GitBackend::Local(repositories) => Some(repositories.clone()),
            GitBackend::Remote(_) | GitBackend::Native(_) => None,
        };

        git_spawn!(cx, |this, cx| {
            let results: Vec<(usize, Result<GitStatusResult, String>)> =
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
                                    local_repositories
                                        .as_ref()
                                        .and_then(|repositories| repositories.get(i))
                                        .ok_or_else(|| {
                                            "Local Git repository is unavailable.".to_string()
                                        })
                                        .and_then(git_service::status)
                                };
                                (i, status)
                            })
                            .collect()
                    })
                    .await;
            let _ = this.update(&mut cx, |this, cx| {
                if this.repo_scan_epoch != scan_epoch {
                    return;
                }
                for (i, status) in results {
                    if let Some(repo) = this.repos.get_mut(i) {
                        repo.apply_status(&status);
                    }
                }
                cx.notify();
            });
        });
    }

    // ── Data loading ────────────────────────────────────────────────────

    pub fn refresh_status(&mut self, cx: &mut Context<Self>) {
        self.refresh_status_inner(false, cx);
    }

    fn refresh_status_inner(&mut self, auto_stage: bool, cx: &mut Context<Self>) {
        if self.repos.is_empty() {
            self.is_loading = false;
            self.operation_result = Some((
                true,
                "No repositories yet. Add a project base folder to see its Git repositories here."
                    .into(),
            ));
            cx.notify();
            return;
        }
        self.status_epoch += 1;
        let status_epoch = self.status_epoch;
        let repo = self.repo_path().to_string();
        let repo_epoch = self.repo_epoch;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
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
                    let repository = local_repository.clone();
                    let _ = cx
                        .background_executor()
                        .spawn(async move {
                            repository
                                .as_ref()
                                .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                                .and_then(git_service::stage_all)
                        })
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(git_service::status)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if this.repo_epoch != repo_epoch || this.status_epoch != status_epoch {
                    return;
                }
                this.is_loading = false;
                if let Some(repo) = this.repos.get_mut(this.active_repo) {
                    repo.apply_status(&status);
                }
                match status {
                    Ok(s) => {
                        this.status = Some(s);
                        let selected = this
                            .selected_file
                            .clone()
                            .filter(|path| {
                                this.status
                                    .as_ref()
                                    .is_some_and(|s| s.entries.iter().any(|e| &e.path == path))
                            })
                            .or_else(|| {
                                this.status
                                    .as_ref()
                                    .and_then(|s| s.entries.first().map(|e| e.path.clone()))
                            });
                        this.selected_file = selected.clone();
                        this.file_diff = None;
                        if let Some(path) = selected {
                            this.select_file(&path, cx);
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
        let repo_epoch = self.repo_epoch;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(git_service::branches)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if this.repo_epoch != repo_epoch {
                    return;
                }
                if let Ok(b) = branches {
                    this.branches = b;
                }
                cx.notify();
            });
        });
    }

    pub fn load_history(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo_path().to_string();
        let repo_epoch = self.repo_epoch;
        let skip = self.log_page * 50;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| git_service::log(repository, 50, skip))
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        let staged = self
            .status
            .as_ref()
            .and_then(|status| {
                status
                    .entries
                    .iter()
                    .find(|entry| entry.path == path && entry.staged == self.selected_file_staged)
                    .or_else(|| status.entries.iter().find(|entry| entry.path == path))
            })
            .map(|entry| entry.staged)
            .unwrap_or(false);
        self.select_file_side(path, staged, cx);
    }

    pub fn select_file_side(&mut self, path: &str, staged: bool, cx: &mut Context<Self>) {
        self.file_diff_epoch += 1;
        let file_diff_epoch = self.file_diff_epoch;
        self.selected_file = Some(path.to_string());
        self.selected_file_staged = staged;
        self.file_diff = None;
        let repo = self.repo_path().to_string();
        let repo_epoch = self.repo_epoch;
        let selected = self.selected_file.clone();
        let file_path = path.to_string();
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();

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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| {
                                git_service::diff_file(repository, &file_path, staged)
                            })
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if this.file_diff_epoch != file_diff_epoch
                    || this.repo_epoch != repo_epoch
                    || this.selected_file != selected
                    || this.selected_file_staged != staged
                {
                    return;
                }
                match diff {
                    Ok(d) => this.file_diff = Some(d),
                    Err(error) => {
                        this.file_diff = Some(GitDiffResult {
                            hunks: Vec::new(),
                            is_binary: false,
                        });
                        this.operation_result = Some((false, error));
                    }
                }
                cx.notify();
            });
        });
    }

    pub fn select_commit(&mut self, hash: &str, cx: &mut Context<Self>) {
        self.selected_commit = Some(hash.to_string());
        self.commit_diff = None;
        let repo = self.repo_path().to_string();
        let repo_epoch = self.repo_epoch;
        let selected = self.selected_commit.clone();
        let hash = hash.to_string();
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();

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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| git_service::diff_commit(repository, &hash))
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if this.repo_epoch != repo_epoch || this.selected_commit != selected {
                    return;
                }
                match diff {
                    Ok(d) => this.commit_diff = Some(d),
                    Err(error) => {
                        this.commit_diff = Some(GitDiffResult {
                            hunks: Vec::new(),
                            is_binary: false,
                        });
                        this.operation_result = Some((false, error));
                    }
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
        let repo_epoch = self.repo_epoch;
        let file_path = path.to_string();
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.is_mutating = true;
        self.draft_epoch += 1;
        cx.notify();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| {
                                git_service::stage(repository, &[file_path.as_str()])
                            })
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        let repo_epoch = self.repo_epoch;
        let file_path = path.to_string();
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.is_mutating = true;
        self.draft_epoch += 1;
        cx.notify();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| {
                                git_service::unstage(repository, &[file_path.as_str()])
                            })
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        let repo_epoch = self.repo_epoch;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.is_mutating = true;
        self.draft_epoch += 1;
        cx.notify();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(git_service::stage_all)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        let repo_epoch = self.repo_epoch;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.is_mutating = true;
        self.draft_epoch += 1;
        cx.notify();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(git_service::unstage_all)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
                if let Err(error) = result {
                    this.operation_result = Some((false, error));
                }
                this.refresh_status(cx);
            });
        });
    }

    // ── Token persistence ────────────────────────────────────────────

    pub fn persist_github_token(token: Option<String>) {
        let availability: persistence::ConfigWriteAvailability =
            persistence::active_config_write_availability();
        if !matches!(availability, persistence::ConfigWriteAvailability::Ready) {
            return;
        }
        // The canonical ConfigStore accepts only credential references.  A
        // raw OAuth token is retained in memory for the current session and is
        // deliberately rejected by the persistence boundary until the
        // credential-provider migration lands.
        let _ = persistence::persist_github_token_reference(token.as_deref());
    }

    fn load_persisted_token(&mut self) {
        // Raw GitHub tokens are never reconstructed from the legacy-facing
        // model.  A future credential provider will resolve the strict opaque
        // reference here without exposing it through diagnostics or export.
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
        if self.is_generating_message || self.is_committing {
            return;
        }
        let draft_epoch = self.draft_epoch;
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.github_token.is_none() {
            self.operation_result = Some((
                false,
                "Sign in to GitHub to generate a commit message.".to_string(),
            ));
            cx.notify();
            return;
        }
        let repo = self.repo_path().to_string();
        let repo_epoch = self.repo_epoch;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
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
                            let repository = local_repository.as_ref().ok_or_else(|| {
                                "Local Git repository is unavailable.".to_string()
                            })?;
                            let diff = git_service::get_staged_diff(repository)?;
                            if diff.trim().is_empty() {
                                return Err("No staged changes to summarize".to_string());
                            }
                            git_service::generate_commit_message(&token, &diff)
                        }
                    })
                    .await;
            let _ = this.update(&mut cx, |this, cx| {
                if this.repo_epoch != repo_epoch { return; }
                this.is_generating_message = false;
                match result {
                    Ok(_) if this.draft_epoch != draft_epoch => {
                        this.operation_result = Some((false, "Your draft changed while AI was writing. Generate again to replace it.".into()));
                    }
                    Ok(msg) => {
                        this.draft_epoch += 1;
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
        if self.is_committing || self.is_generating_message || self.staged_count() == 0 {
            return;
        }
        let draft_epoch = self.draft_epoch;
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.commit_summary.trim().is_empty() {
            self.operation_result = Some((false, "Summary is required".to_string()));
            cx.notify();
            return;
        }
        let repo = self.repo_path().to_string();
        let repo_epoch = self.repo_epoch;
        let summary = self.commit_summary.clone();
        let body = if self.commit_description.trim().is_empty() {
            None
        } else {
            Some(self.commit_description.clone())
        };
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();

        self.is_committing = true;
        cx.notify();
        self.is_mutating = true;
        cx.notify();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| {
                                git_service::commit(repository, &summary, body.as_deref())
                            })
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
                this.is_committing = false;
                match result {
                    Ok(hash) => {
                        this.operation_result = Some((true, format!("Committed {}", hash)));
                        if this.draft_epoch == draft_epoch {
                            this.commit_summary.clear();
                            this.commit_description.clear();
                            this.draft_epoch += 1;
                        }
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
        let repo_epoch = self.repo_epoch;
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
        let local_repository = self.local_repository();
        self.is_pushing = true;
        cx.notify();

        self.is_mutating = true;
        self.draft_epoch += 1;
        cx.notify();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        let action = if has_upstream {
                            RemoteAction::GitSync { repo_path: repo }
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
                        let repository = local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())?;
                        if has_upstream {
                            git_service::sync(repository)
                        } else {
                            git_service::push_set_upstream(repository, &branch)
                        }
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        let repo_epoch = self.repo_epoch;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.is_pulling = true;
        cx.notify();
        self.is_mutating = true;
        self.draft_epoch += 1;
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(git_service::pull)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        let repo_epoch = self.repo_epoch;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.is_fetching = true;
        cx.notify();
        self.is_mutating = true;
        self.draft_epoch += 1;
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(git_service::fetch)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
                this.is_fetching = false;
                match result {
                    Ok(_) => {
                        this.last_fetch_at = Some(Instant::now());
                        this.refresh_status(cx);
                    }
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
        let repo_epoch = self.repo_epoch;
        let branch = name.to_string();
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.show_branch_dropdown = false;
        self.is_mutating = true;
        self.draft_epoch += 1;
        cx.notify();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| git_service::switch_branch(repository, &branch))
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        let repo_epoch = self.repo_epoch;
        let name = self.new_branch_name.trim().to_string();
        self.new_branch_name.clear();
        self.show_branch_dropdown = false;
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.is_mutating = true;
        self.draft_epoch += 1;
        cx.notify();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| git_service::create_branch(repository, &name))
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        let repo_epoch = self.repo_epoch;
        let branch = name.to_string();
        let remote_client = self.remote_client();
        let local_repository = self.local_repository();
        self.is_mutating = true;
        self.draft_epoch += 1;
        cx.notify();
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
                        local_repository
                            .as_ref()
                            .ok_or_else(|| "Local Git repository is unavailable.".to_string())
                            .and_then(|repository| git_service::delete_branch(repository, &branch))
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_mutating = false;
                if this.repo_epoch != repo_epoch {
                    return;
                }
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
        if matches!(
            self.active_field,
            Some(GitField::CommitSummary | GitField::CommitDescription)
        ) {
            self.draft_epoch += 1;
        }
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
            let mut cursor = self.cursor.min(current_len);
            while !current.is_char_boundary(cursor) {
                cursor -= 1;
            }
            let previous = current[..cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            let next = current[cursor..]
                .chars()
                .next()
                .map(|ch| cursor + ch.len_utf8())
                .unwrap_or(cursor);

            if keystroke.modifiers.control && keystroke.key == "v" {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let text = if matches!(self.active_field, Some(GitField::CommitDescription)) {
                        text
                    } else {
                        text.replace(['\n', '\r'], " ")
                    };
                    let mut value = current;
                    value.insert_str(cursor, &text);
                    self.cursor = cursor + text.len();
                    self.apply_text(value);
                    cx.notify();
                }
            } else if keystroke.modifiers.control && keystroke.key == "backspace" {
                let start = current[..cursor]
                    .trim_end()
                    .rfind(char::is_whitespace)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let mut value = current;
                value.replace_range(start..cursor, "");
                self.cursor = start;
                self.apply_text(value);
                cx.notify();
            } else if keystroke.key == "backspace" {
                if cursor > 0 {
                    let mut v = current;
                    v.remove(previous);
                    self.cursor = previous;
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
                    self.cursor = previous;
                    cx.notify();
                }
            } else if keystroke.key == "right" {
                if cursor < current_len {
                    self.cursor = next;
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
                if keystroke.modifiers.control || keystroke.modifiers.platform {
                    return;
                }
                v.insert_str(cursor, text);
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((summary, description)) = &self.commit_inputs {
            for (input, value) in [
                (summary, &self.commit_summary),
                (description, &self.commit_description),
            ] {
                if input.read(cx).value().as_str() != value {
                    input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
                }
            }
        }
        let show_branch_dd = self.show_branch_dropdown;
        let show_repo_dd = self.show_repo_dropdown;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(self.tokens.surfaces.canvas.to_gpui())
            .text_color(self.tokens.text.primary.to_gpui())
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
