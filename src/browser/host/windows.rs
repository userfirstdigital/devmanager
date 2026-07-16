use super::{
    browser_user_input_initialization_script, unique_download_path, validate_browser_url,
    BrowserHostState, BrowserMemoryTarget,
};
use crate::browser::{
    BrowserBounds, BrowserCommand, BrowserCommandRequest, BrowserDiagnosticLevel,
    BrowserDownloadState, BrowserError, BrowserHostEvent, BrowserHostStatus, BrowserPageLoadState,
    BrowserResponse, BrowserStorageLayout, BrowserUserInputKind, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{
    MemoryUsageLevel, NewWindowResponse, PageLoadEvent, Rect, WebContext, WebView, WebViewBuilder,
    WebViewExtWindows,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BrowserViewKey {
    workspace_key: BrowserWorkspaceKey,
    tab_id: String,
}

struct BrowserProjectRuntime {
    layout: BrowserStorageLayout,
    context: WebContext,
}

pub struct BrowserWebViewHost {
    status: BrowserHostStatus,
    app_config_dir: PathBuf,
    state: BrowserHostState,
    projects: HashMap<String, BrowserProjectRuntime>,
    views: HashMap<BrowserViewKey, WebView>,
    bounds: BrowserBounds,
    event_sender: Sender<BrowserHostEvent>,
    event_receiver: Receiver<BrowserHostEvent>,
    _main_thread_only: PhantomData<Rc<()>>,
}

impl BrowserWebViewHost {
    pub fn new(app_config_dir: impl AsRef<Path>) -> Self {
        let app_config_dir = absolute_path(app_config_dir.as_ref());
        let status = match wry::webview_version() {
            Ok(version) => BrowserHostStatus {
                available: true,
                platform: std::env::consts::OS.to_string(),
                version: Some(version),
                diagnostic: None,
            },
            Err(error) => BrowserHostStatus {
                available: false,
                platform: std::env::consts::OS.to_string(),
                version: None,
                diagnostic: Some(format!("WebView2 runtime is unavailable: {error}")),
            },
        };
        let (event_sender, event_receiver) = mpsc::channel();
        Self {
            status,
            state: BrowserHostState::new(&app_config_dir),
            app_config_dir,
            projects: HashMap::new(),
            views: HashMap::new(),
            bounds: BrowserBounds {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            event_sender,
            event_receiver,
            _main_thread_only: PhantomData,
        }
    }

    pub fn status(&self) -> BrowserHostStatus {
        self.status.clone()
    }

    pub fn handle_command(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        let diagnostic_tab = command
            .tab_id()
            .map(ToOwned::to_owned)
            .or_else(|| self.selected_tab_id(workspace_key));
        let result = self.handle_command_inner(window, workspace_key, command);
        if let Err(error) = &result {
            if let Some(tab_id) = diagnostic_tab.or_else(|| self.selected_tab_id(workspace_key)) {
                self.emit_diagnostic(workspace_key, &tab_id, error.to_string());
            }
        }
        result
    }

    pub fn handle_request(&mut self, window: &gpui::Window, request: BrowserCommandRequest) {
        let result =
            self.handle_command(window, request.workspace_key(), request.command().clone());
        request.respond(result);
    }

    pub fn set_active_workspace(
        &mut self,
        workspace_key: Option<BrowserWorkspaceKey>,
    ) -> Result<(), BrowserError> {
        self.state.set_active_workspace(workspace_key);
        self.apply_visibility_plan()
    }

    pub fn set_bounds(&mut self, bounds: BrowserBounds) -> Result<(), BrowserError> {
        self.bounds = BrowserBounds {
            width: bounds.width.max(1),
            height: bounds.height.max(1),
            ..bounds
        };
        self.apply_visibility_plan()
    }

    pub fn drain_events(&mut self) -> Vec<BrowserHostEvent> {
        let events: Vec<_> = self.event_receiver.try_iter().collect();
        for event in &events {
            match event {
                BrowserHostEvent::UrlChanged {
                    workspace_key,
                    tab_id,
                    url,
                } => {
                    let _ = self.state.navigate_tab(workspace_key, tab_id, url);
                }
                BrowserHostEvent::TitleChanged {
                    workspace_key,
                    tab_id,
                    title,
                } => {
                    let _ = self.state.apply_title_change(workspace_key, tab_id, title);
                }
                BrowserHostEvent::PageLoad {
                    workspace_key,
                    tab_id,
                    state: BrowserPageLoadState::Finished,
                    url,
                } => {
                    let _ = self.state.apply_page_load(workspace_key, tab_id, url);
                }
                BrowserHostEvent::UserInput {
                    workspace_key,
                    tab_id,
                    ..
                } => {
                    let _ = self.state.apply_user_input(workspace_key, tab_id);
                }
                BrowserHostEvent::PageLoad { .. }
                | BrowserHostEvent::NewWindow { .. }
                | BrowserHostEvent::Download { .. }
                | BrowserHostEvent::Diagnostic { .. } => {}
            }
        }
        events
    }

    pub fn workspace_snapshot(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<&BrowserWorkspaceSnapshot> {
        self.state.workspace(workspace_key)
    }

    fn handle_command_inner(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        match command {
            BrowserCommand::Status => Ok(BrowserResponse::Status {
                status: self.status(),
            }),
            BrowserCommand::DownloadDirectory => {
                let layout =
                    BrowserStorageLayout::new(&self.app_config_dir, &workspace_key.project_id);
                std::fs::create_dir_all(&layout.downloads_dir).map_err(|error| {
                    BrowserError::Io {
                        operation: "create browser download directory".to_string(),
                        path: layout.downloads_dir.clone(),
                        message: error.to_string(),
                    }
                })?;
                Ok(BrowserResponse::DownloadDirectory {
                    path: layout.downloads_dir,
                })
            }
            BrowserCommand::ClearProjectProfile => {
                self.clear_project_profile(workspace_key)?;
                Ok(BrowserResponse::Acknowledged)
            }
            command => {
                self.ensure_runtime_available()?;
                self.handle_available_command(window, workspace_key, command)
            }
        }
    }

    fn handle_available_command(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        match command {
            BrowserCommand::Ensure { snapshot } => {
                let mutation = self
                    .state
                    .ensure_workspace(workspace_key.clone(), snapshot)?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::SetPaneOpen { open } => {
                let mutation = self.state.set_pane_open(workspace_key, open)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::ListTabs => {
                let snapshot = self
                    .state
                    .workspace(workspace_key)
                    .ok_or_else(missing_workspace)?;
                Ok(BrowserResponse::Tabs {
                    tabs: snapshot.tabs.clone(),
                    selected_tab_id: snapshot.selected_tab_id.clone(),
                })
            }
            BrowserCommand::CreateTab { url } => {
                let mutation = self
                    .state
                    .create_tab(workspace_key, url.as_deref().unwrap_or("about:blank"))?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::SelectTab { tab_id } => {
                let mutation = self.state.select_tab(workspace_key, &tab_id)?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::CloseTab { tab_id } => {
                let key = view_key(workspace_key, &tab_id);
                self.views.remove(&key);
                let mutation = self.state.close_tab(workspace_key, &tab_id)?;
                self.ensure_selected_view(window, workspace_key)?;
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::Navigate { tab_id, url } => {
                let url = validate_browser_url(&url)?;
                self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                self.view(workspace_key, &tab_id)?
                    .load_url(&url)
                    .map_err(|error| BrowserError::NavigationFailure {
                        url: url.clone(),
                        message: error.to_string(),
                    })?;
                let mutation = self.state.navigate_tab(workspace_key, &tab_id, &url)?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::Back { tab_id } => {
                self.evaluate_history(window, workspace_key, &tab_id, "history.back()")?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Forward { tab_id } => {
                self.evaluate_history(window, workspace_key, &tab_id, "history.forward()")?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Reload { tab_id } => {
                self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                self.view(workspace_key, &tab_id)?
                    .reload()
                    .map_err(view_failure)?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::UpdateViewport { tab_id, viewport } => {
                let mutation = self
                    .state
                    .update_viewport(workspace_key, &tab_id, viewport)?;
                Ok(BrowserResponse::Workspace { mutation })
            }
            BrowserCommand::OpenDevTools { tab_id } => {
                self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                self.view(workspace_key, &tab_id)?.open_devtools();
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Stop { tab_id } => {
                if let Some(tab_id) = tab_id {
                    self.ensure_existing_tab_view(window, workspace_key, &tab_id)?;
                    self.view(workspace_key, &tab_id)?
                        .evaluate_script("window.stop()")
                        .map_err(view_failure)?;
                } else {
                    for (key, view) in &self.views {
                        if key.workspace_key == *workspace_key {
                            view.evaluate_script("window.stop()")
                                .map_err(view_failure)?;
                        }
                    }
                }
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::ResetWorkspace => {
                self.views
                    .retain(|key, _| key.workspace_key != *workspace_key);
                self.state.reset_workspace(workspace_key);
                self.apply_visibility_plan()?;
                Ok(BrowserResponse::Acknowledged)
            }
            BrowserCommand::Status
            | BrowserCommand::DownloadDirectory
            | BrowserCommand::ClearProjectProfile => unreachable!("handled before availability"),
        }
    }

    fn ensure_runtime_available(&self) -> Result<(), BrowserError> {
        if self.status.available {
            Ok(())
        } else {
            Err(BrowserError::CrashedView {
                message: self
                    .status
                    .diagnostic
                    .clone()
                    .unwrap_or_else(|| "WebView2 runtime is unavailable".to_string()),
            })
        }
    }

    fn ensure_selected_view(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<(), BrowserError> {
        let plan = self
            .state
            .selected_view_plan(workspace_key)
            .ok_or_else(missing_workspace)?;
        self.ensure_view(window, workspace_key, &plan.tab_id, &plan.url)
    }

    fn ensure_existing_tab_view(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<(), BrowserError> {
        let url = self
            .state
            .workspace(workspace_key)
            .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.id == tab_id))
            .map(|tab| tab.url.clone())
            .ok_or_else(|| missing_tab(tab_id))?;
        self.ensure_view(window, workspace_key, tab_id, &url)
    }

    fn ensure_view(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        url: &str,
    ) -> Result<(), BrowserError> {
        let key = view_key(workspace_key, tab_id);
        if self.views.contains_key(&key) {
            return Ok(());
        }
        let url = validate_browser_url(url)?;
        let layout = BrowserStorageLayout::new(&self.app_config_dir, &workspace_key.project_id);
        layout.ensure()?;
        self.projects
            .entry(workspace_key.project_id.clone())
            .or_insert_with(|| BrowserProjectRuntime {
                context: WebContext::new(Some(layout.profile_dir.clone())),
                layout: layout.clone(),
            });

        let sender = self.event_sender.clone();
        let callback_workspace = workspace_key.clone();
        let callback_tab = tab_id.to_string();
        let downloads_dir = self
            .projects
            .get(&workspace_key.project_id)
            .ok_or_else(|| BrowserError::CrashedView {
                message: "browser project context was not initialized".to_string(),
            })?
            .layout
            .downloads_dir
            .clone();
        let bounds = wry_bounds(self.bounds);
        let webview = {
            let project = self
                .projects
                .get_mut(&workspace_key.project_id)
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser project context was not initialized".to_string(),
                })?;
            let builder = configured_builder(
                &mut project.context,
                sender,
                callback_workspace,
                callback_tab,
                downloads_dir,
                url,
                bounds,
            );
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                builder.build_as_child(window)
            })) {
                Ok(Ok(webview)) => webview,
                Ok(Err(error)) => return Err(view_failure(error)),
                Err(payload) => {
                    return Err(BrowserError::CrashedView {
                        message: format!(
                            "Wry panicked while creating a child WebView: {}",
                            panic_message(payload)
                        ),
                    })
                }
            }
        };
        webview.set_visible(false).map_err(view_failure)?;
        webview
            .set_memory_usage_level(MemoryUsageLevel::Low)
            .map_err(view_failure)?;
        self.views.insert(key, webview);
        Ok(())
    }

    fn evaluate_history(
        &mut self,
        window: &gpui::Window,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        script: &str,
    ) -> Result<(), BrowserError> {
        self.ensure_existing_tab_view(window, workspace_key, tab_id)?;
        self.view(workspace_key, tab_id)?
            .evaluate_script(script)
            .map_err(view_failure)
    }

    fn view(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<&WebView, BrowserError> {
        self.views
            .get(&view_key(workspace_key, tab_id))
            .ok_or_else(|| missing_tab(tab_id))
    }

    fn selected_tab_id(&self, workspace_key: &BrowserWorkspaceKey) -> Option<String> {
        self.state
            .workspace(workspace_key)
            .and_then(|snapshot| snapshot.selected_tab_id.clone())
    }

    fn apply_visibility_plan(&mut self) -> Result<(), BrowserError> {
        let plans = self.state.visibility_plan();
        let mut first_error = None;
        let mut diagnostics = Vec::new();
        for plan in plans {
            let Some(view) = self.views.get(&view_key(&plan.workspace_key, &plan.tab_id)) else {
                continue;
            };
            let result = if plan.visible {
                view.set_bounds(wry_bounds(self.bounds))
                    .and_then(|_| view.set_memory_usage_level(MemoryUsageLevel::Normal))
                    .and_then(|_| view.set_visible(true))
            } else {
                view.set_visible(false)
                    .and_then(|_| view.set_memory_usage_level(MemoryUsageLevel::Low))
            };
            if let Err(error) = result {
                let message = format!("could not update WebView visibility: {error}");
                diagnostics.push((plan.workspace_key, plan.tab_id, message.clone()));
                first_error.get_or_insert_with(|| BrowserError::CrashedView { message });
            }
            debug_assert_eq!(
                plan.memory_target,
                if plan.visible {
                    BrowserMemoryTarget::Normal
                } else {
                    BrowserMemoryTarget::Low
                }
            );
        }
        for (workspace_key, tab_id, message) in diagnostics {
            self.emit_diagnostic(&workspace_key, &tab_id, message);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn clear_project_profile(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<(), BrowserError> {
        let layout = BrowserStorageLayout::new(&self.app_config_dir, &workspace_key.project_id);
        let plan = self
            .state
            .profile_clear_plan(workspace_key, &layout.profile_dir)?;

        self.views
            .retain(|key, _| key.workspace_key.project_id != workspace_key.project_id);
        self.projects.remove(&workspace_key.project_id);
        self.state
            .clear_project_workspaces(&workspace_key.project_id);
        remove_verified_profile(&self.app_config_dir, &plan.profile_dir)
    }

    fn emit_diagnostic(&self, workspace_key: &BrowserWorkspaceKey, tab_id: &str, message: String) {
        let _ = self.event_sender.send(BrowserHostEvent::Diagnostic {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.to_string(),
            level: BrowserDiagnosticLevel::Error,
            message,
        });
    }
}

fn configured_builder<'a>(
    context: &'a mut WebContext,
    event_sender: Sender<BrowserHostEvent>,
    workspace_key: BrowserWorkspaceKey,
    tab_id: String,
    downloads_dir: PathBuf,
    url: String,
    bounds: Rect,
) -> WebViewBuilder<'a> {
    let navigation_sender = event_sender.clone();
    let navigation_workspace = workspace_key.clone();
    let navigation_tab = tab_id.clone();
    let title_sender = event_sender.clone();
    let title_workspace = workspace_key.clone();
    let title_tab = tab_id.clone();
    let load_sender = event_sender.clone();
    let load_workspace = workspace_key.clone();
    let load_tab = tab_id.clone();
    let ipc_sender = event_sender.clone();
    let ipc_workspace = workspace_key.clone();
    let ipc_tab = tab_id.clone();
    let window_sender = event_sender.clone();
    let window_workspace = workspace_key.clone();
    let window_tab = tab_id.clone();
    let download_sender = event_sender.clone();
    let download_workspace = workspace_key.clone();
    let download_tab = tab_id.clone();
    let completion_workspace = workspace_key;
    let completion_tab = tab_id;
    let completion_downloads_dir = downloads_dir.clone();

    WebViewBuilder::new_with_web_context(context)
        .with_url(url)
        .with_bounds(bounds)
        .with_visible(false)
        .with_focused(false)
        .with_clipboard(true)
        .with_initialization_script(browser_user_input_initialization_script())
        .with_navigation_handler(move |url| match validate_browser_url(&url) {
            Ok(_) => {
                let _ = navigation_sender.send(BrowserHostEvent::UrlChanged {
                    workspace_key: navigation_workspace.clone(),
                    tab_id: navigation_tab.clone(),
                    url,
                });
                true
            }
            Err(error) => {
                let _ = navigation_sender.send(BrowserHostEvent::Diagnostic {
                    workspace_key: navigation_workspace.clone(),
                    tab_id: navigation_tab.clone(),
                    level: BrowserDiagnosticLevel::Warning,
                    message: error.to_string(),
                });
                false
            }
        })
        .with_document_title_changed_handler(move |title| {
            let _ = title_sender.send(BrowserHostEvent::TitleChanged {
                workspace_key: title_workspace.clone(),
                tab_id: title_tab.clone(),
                title,
            });
        })
        .with_on_page_load_handler(move |state, url| {
            let state = match state {
                PageLoadEvent::Started => BrowserPageLoadState::Started,
                PageLoadEvent::Finished => BrowserPageLoadState::Finished,
            };
            let _ = load_sender.send(BrowserHostEvent::PageLoad {
                workspace_key: load_workspace.clone(),
                tab_id: load_tab.clone(),
                state,
                url,
            });
        })
        .with_ipc_handler(move |request| {
            let event = match serde_json::from_str::<BrowserInputMessage>(request.body()) {
                Ok(BrowserInputMessage::UserInput { kind }) => BrowserHostEvent::UserInput {
                    workspace_key: ipc_workspace.clone(),
                    tab_id: ipc_tab.clone(),
                    kind,
                },
                Err(_) => BrowserHostEvent::Diagnostic {
                    workspace_key: ipc_workspace.clone(),
                    tab_id: ipc_tab.clone(),
                    level: BrowserDiagnosticLevel::Warning,
                    message: "ignored malformed browser input metadata".to_string(),
                },
            };
            let _ = ipc_sender.send(event);
        })
        .with_new_window_req_handler(move |url, _features| {
            let _ = window_sender.send(BrowserHostEvent::NewWindow {
                workspace_key: window_workspace.clone(),
                tab_id: window_tab.clone(),
                url,
            });
            NewWindowResponse::Deny
        })
        .with_download_started_handler(move |url, suggested_path| {
            match unique_download_path(&downloads_dir, &*suggested_path) {
                Ok(path) => {
                    *suggested_path = path.clone();
                    let _ = download_sender.send(BrowserHostEvent::Download {
                        workspace_key: download_workspace.clone(),
                        tab_id: download_tab.clone(),
                        state: BrowserDownloadState::Started,
                        url,
                        path,
                    });
                    true
                }
                Err(error) => {
                    let _ = download_sender.send(BrowserHostEvent::Diagnostic {
                        workspace_key: download_workspace.clone(),
                        tab_id: download_tab.clone(),
                        level: BrowserDiagnosticLevel::Error,
                        message: error.to_string(),
                    });
                    false
                }
            }
        })
        .with_download_completed_handler(move |url, path, successful| {
            let _ = event_sender.send(BrowserHostEvent::Download {
                workspace_key: completion_workspace.clone(),
                tab_id: completion_tab.clone(),
                state: BrowserDownloadState::Completed { successful },
                url,
                path: path.unwrap_or_else(|| completion_downloads_dir.clone()),
            });
        })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
enum BrowserInputMessage {
    UserInput { kind: BrowserUserInputKind },
}

fn view_key(workspace_key: &BrowserWorkspaceKey, tab_id: &str) -> BrowserViewKey {
    BrowserViewKey {
        workspace_key: workspace_key.clone(),
        tab_id: tab_id.to_string(),
    }
}

fn wry_bounds(bounds: BrowserBounds) -> Rect {
    Rect {
        position: LogicalPosition::new(bounds.x, bounds.y).into(),
        size: LogicalSize::new(bounds.width.max(1), bounds.height.max(1)).into(),
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn remove_verified_profile(app_config_dir: &Path, profile_dir: &Path) -> Result<(), BrowserError> {
    if !profile_dir.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(profile_dir).map_err(|error| BrowserError::Io {
        operation: "inspect browser profile directory".to_string(),
        path: profile_dir.to_path_buf(),
        message: error.to_string(),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BrowserError::OutsideWorkspace {
            path: profile_dir.to_path_buf(),
        });
    }
    let canonical_app = app_config_dir
        .canonicalize()
        .map_err(|error| BrowserError::Io {
            operation: "verify browser app data directory".to_string(),
            path: app_config_dir.to_path_buf(),
            message: error.to_string(),
        })?;
    let canonical_profile = profile_dir
        .canonicalize()
        .map_err(|error| BrowserError::Io {
            operation: "verify browser profile directory".to_string(),
            path: profile_dir.to_path_buf(),
            message: error.to_string(),
        })?;
    let canonical_parent = profile_dir
        .parent()
        .ok_or_else(|| BrowserError::OutsideWorkspace {
            path: profile_dir.to_path_buf(),
        })?
        .canonicalize()
        .map_err(|error| BrowserError::Io {
            operation: "verify browser profiles root".to_string(),
            path: profile_dir.to_path_buf(),
            message: error.to_string(),
        })?;
    let verified = canonical_parent.starts_with(&canonical_app)
        && canonical_profile.parent() == Some(canonical_parent.as_path())
        && canonical_profile.file_name() == profile_dir.file_name();
    if !verified {
        return Err(BrowserError::OutsideWorkspace {
            path: profile_dir.to_path_buf(),
        });
    }
    std::fs::remove_dir_all(&canonical_profile).map_err(|error| BrowserError::Io {
        operation: "clear browser project profile".to_string(),
        path: canonical_profile,
        message: error.to_string(),
    })
}

fn missing_workspace() -> BrowserError {
    BrowserError::CrashedView {
        message: "browser workspace has not been ensured".to_string(),
    }
}

fn missing_tab(tab_id: &str) -> BrowserError {
    BrowserError::CrashedView {
        message: format!("browser tab {tab_id:?} does not exist"),
    }
}

fn view_failure(error: impl std::fmt::Display) -> BrowserError {
    BrowserError::CrashedView {
        message: error.to_string(),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}
