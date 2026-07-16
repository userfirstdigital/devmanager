use super::{
    BrowserError, BrowserRevision, BrowserStorageLayout, BrowserTabSnapshot, BrowserViewport,
    BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
mod initialization;
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "windows"))]
pub use unsupported::BrowserWebViewHost;
pub use unsupported::{unsupported_host_status, unsupported_platform_error};
#[cfg(target_os = "windows")]
pub use windows::BrowserWebViewHost;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserWorkspaceMutation {
    pub revision: BrowserRevision,
    pub snapshot: BrowserWorkspaceSnapshot,
}

impl BrowserWorkspaceMutation {
    fn new(snapshot: BrowserWorkspaceSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            snapshot,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewCreationPlan {
    pub workspace_key: BrowserWorkspaceKey,
    pub tab_id: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserMemoryTarget {
    Normal,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewVisibilityPlan {
    pub workspace_key: BrowserWorkspaceKey,
    pub tab_id: String,
    pub visible: bool,
    pub memory_target: BrowserMemoryTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrowserProjectContextKey {
    pub project_id: String,
    pub profile_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProfileClearPlan {
    pub profile_dir: PathBuf,
}

impl BrowserProfileClearPlan {
    pub fn paths(&self) -> [&Path; 1] {
        [self.profile_dir.as_path()]
    }
}

pub struct BrowserHostState {
    app_config_dir: PathBuf,
    workspaces: HashMap<BrowserWorkspaceKey, BrowserWorkspaceSnapshot>,
    active_workspace: Option<BrowserWorkspaceKey>,
}

impl BrowserHostState {
    pub fn new(app_config_dir: impl AsRef<Path>) -> Self {
        Self {
            app_config_dir: app_config_dir.as_ref().to_path_buf(),
            workspaces: HashMap::new(),
            active_workspace: None,
        }
    }

    pub fn ensure_workspace(
        &mut self,
        workspace_key: BrowserWorkspaceKey,
        mut snapshot: BrowserWorkspaceSnapshot,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        if let Some(existing) = self.workspaces.get(&workspace_key) {
            return Ok(BrowserWorkspaceMutation::new(existing.clone()));
        }
        let mut changed = false;
        if snapshot.tabs.is_empty() {
            let tab_id = self.generate_tab_id()?;
            snapshot.tabs.push(BrowserTabSnapshot {
                id: tab_id.clone(),
                title: String::new(),
                url: "about:blank".to_string(),
                viewport: BrowserViewport::default(),
            });
            snapshot.selected_tab_id = Some(tab_id);
            changed = true;
        } else if snapshot
            .selected_tab_id
            .as_ref()
            .is_none_or(|selected| !snapshot.tabs.iter().any(|tab| &tab.id == selected))
        {
            snapshot.selected_tab_id = snapshot.tabs.first().map(|tab| tab.id.clone());
            changed = true;
        }
        if changed {
            snapshot.advance_revision();
        }
        self.workspaces.insert(workspace_key, snapshot.clone());
        Ok(BrowserWorkspaceMutation::new(snapshot))
    }

    pub fn create_tab(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        url: impl Into<String>,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let url = validate_browser_url(&url.into())?;
        let tab_id = self.generate_tab_id()?;
        let snapshot =
            self.workspaces
                .get_mut(workspace_key)
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser workspace has not been ensured".to_string(),
                })?;
        snapshot.tabs.push(BrowserTabSnapshot {
            id: tab_id.clone(),
            title: String::new(),
            url,
            viewport: BrowserViewport::default(),
        });
        snapshot.selected_tab_id = Some(tab_id);
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn select_tab(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(missing_tab(tab_id));
        }
        if snapshot.selected_tab_id.as_deref() != Some(tab_id) {
            snapshot.selected_tab_id = Some(tab_id.to_string());
            snapshot.advance_revision();
        }
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn close_tab(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let existing = self
            .workspaces
            .get(workspace_key)
            .ok_or_else(|| missing_workspace())?;
        let position = existing
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        let replacement_id = if existing.tabs.len() == 1 {
            Some(self.generate_tab_id()?)
        } else {
            None
        };
        let snapshot = self.workspace_mut(workspace_key)?;
        let was_selected = snapshot.selected_tab_id.as_deref() == Some(tab_id);
        snapshot.tabs.remove(position);
        if let Some(replacement_id) = replacement_id {
            snapshot.tabs.push(BrowserTabSnapshot {
                id: replacement_id.clone(),
                title: String::new(),
                url: "about:blank".to_string(),
                viewport: BrowserViewport::default(),
            });
            snapshot.selected_tab_id = Some(replacement_id);
        } else if was_selected {
            let selected_position = position.min(snapshot.tabs.len().saturating_sub(1));
            snapshot.selected_tab_id = snapshot
                .tabs
                .get(selected_position)
                .map(|tab| tab.id.clone());
        }
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn navigate_tab(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        url: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let url = validate_browser_url(url)?;
        let snapshot = self.workspace_mut(workspace_key)?;
        let tab = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        if tab.url != url {
            tab.url = url;
            snapshot.advance_revision();
        }
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn update_viewport(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        viewport: BrowserViewport,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        let tab = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        if tab.viewport != viewport {
            tab.viewport = viewport;
            snapshot.advance_revision();
        }
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_title_change(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        title: impl Into<String>,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        let tab = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        tab.title = title.into();
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_user_input(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(missing_tab(tab_id));
        }
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_dom_mutation(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(missing_tab(tab_id));
        }
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_automation_mutation(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        self.apply_dom_mutation(workspace_key, tab_id)
    }

    pub fn apply_page_load(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        url: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let url = validate_browser_url(url)?;
        let snapshot = self.workspace_mut(workspace_key)?;
        let tab = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        tab.url = url;
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn reset_workspace(&mut self, workspace_key: &BrowserWorkspaceKey) {
        self.workspaces.remove(workspace_key);
        if self.active_workspace.as_ref() == Some(workspace_key) {
            self.active_workspace = None;
        }
    }

    pub fn clear_project_workspaces(&mut self, project_id: &str) {
        self.workspaces
            .retain(|workspace_key, _| workspace_key.project_id != project_id);
        if self
            .active_workspace
            .as_ref()
            .is_some_and(|workspace_key| workspace_key.project_id == project_id)
        {
            self.active_workspace = None;
        }
    }

    pub fn workspace(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<&BrowserWorkspaceSnapshot> {
        self.workspaces.get(workspace_key)
    }

    fn workspace_mut(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<&mut BrowserWorkspaceSnapshot, BrowserError> {
        self.workspaces
            .get_mut(workspace_key)
            .ok_or_else(missing_workspace)
    }

    pub fn selected_view_plan(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<BrowserViewCreationPlan> {
        let snapshot = self.workspaces.get(workspace_key)?;
        let selected = snapshot.selected_tab_id.as_deref()?;
        let tab = snapshot.tabs.iter().find(|tab| tab.id == selected)?;
        Some(BrowserViewCreationPlan {
            workspace_key: workspace_key.clone(),
            tab_id: tab.id.clone(),
            url: tab.url.clone(),
        })
    }

    pub fn project_context_key(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> BrowserProjectContextKey {
        BrowserProjectContextKey {
            project_id: workspace_key.project_id.clone(),
            profile_dir: BrowserStorageLayout::new(&self.app_config_dir, &workspace_key.project_id)
                .profile_dir,
        }
    }

    pub fn set_pane_open(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        open: bool,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot =
            self.workspaces
                .get_mut(workspace_key)
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser workspace has not been ensured".to_string(),
                })?;
        if snapshot.pane_open != open {
            snapshot.pane_open = open;
            snapshot.advance_revision();
        }
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn set_active_workspace(&mut self, workspace_key: Option<BrowserWorkspaceKey>) {
        self.active_workspace = workspace_key;
    }

    pub fn visibility_plan(&self) -> Vec<BrowserViewVisibilityPlan> {
        let mut plans = Vec::new();
        for (workspace_key, snapshot) in &self.workspaces {
            let workspace_is_visible =
                self.active_workspace.as_ref() == Some(workspace_key) && snapshot.pane_open;
            for tab in &snapshot.tabs {
                let visible = workspace_is_visible
                    && snapshot.selected_tab_id.as_deref() == Some(tab.id.as_str());
                plans.push(BrowserViewVisibilityPlan {
                    workspace_key: workspace_key.clone(),
                    tab_id: tab.id.clone(),
                    visible,
                    memory_target: if visible {
                        BrowserMemoryTarget::Normal
                    } else {
                        BrowserMemoryTarget::Low
                    },
                });
            }
        }
        plans
    }

    pub fn profile_clear_plan(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        candidate: impl AsRef<Path>,
    ) -> Result<BrowserProfileClearPlan, BrowserError> {
        let expected =
            BrowserStorageLayout::new(&self.app_config_dir, &workspace_key.project_id).profile_dir;
        let candidate = candidate.as_ref();
        let hash_is_valid = expected
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| {
                value.len() == 64
                    && value.chars().all(|character| {
                        character.is_ascii_digit() || ('a'..='f').contains(&character)
                    })
            });
        if candidate != expected || !hash_is_valid {
            return Err(BrowserError::OutsideWorkspace {
                path: candidate.to_path_buf(),
            });
        }
        Ok(BrowserProfileClearPlan {
            profile_dir: expected,
        })
    }

    fn generate_tab_id(&self) -> Result<String, BrowserError> {
        loop {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|error| BrowserError::CrashedView {
                message: format!("could not generate browser tab id: {error}"),
            })?;
            let mut id = String::with_capacity(36);
            id.push_str("tab-");
            for byte in random {
                let _ = write!(id, "{byte:02x}");
            }
            if self
                .workspaces
                .values()
                .all(|snapshot| snapshot.tabs.iter().all(|tab| tab.id != id))
            {
                return Ok(id);
            }
        }
    }
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

pub fn validate_browser_url(url: &str) -> Result<String, BrowserError> {
    let failure = |message: &str| BrowserError::NavigationFailure {
        url: url.to_string(),
        message: message.to_string(),
    };
    if url.is_empty() || url.trim() != url || url.chars().any(char::is_whitespace) {
        return Err(failure("URL contains empty or whitespace input"));
    }
    if url.eq_ignore_ascii_case("about:blank") {
        return Ok(url.to_string());
    }
    let Some((scheme, remainder)) = url.split_once("://") else {
        return Err(failure("URL must use http, https, or about:blank"));
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Err(failure("URL scheme is not allowed"));
    }
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('\\') {
        return Err(failure("URL must contain a valid network host"));
    }
    Ok(url.to_string())
}

pub fn unique_download_path(
    downloads_dir: impl AsRef<Path>,
    suggested_path: impl AsRef<Path>,
) -> Result<PathBuf, BrowserError> {
    let downloads_dir = downloads_dir.as_ref();
    std::fs::create_dir_all(downloads_dir).map_err(|error| BrowserError::Io {
        operation: "create browser download directory".to_string(),
        path: downloads_dir.to_path_buf(),
        message: error.to_string(),
    })?;
    let suggested_name = suggested_path
        .as_ref()
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("download"));
    let direct = downloads_dir.join(suggested_name);
    if !direct.exists() {
        return Ok(direct);
    }

    let suggested = Path::new(suggested_name);
    let stem = suggested
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("download"))
        .to_string_lossy();
    let extension = suggested.extension().map(|value| value.to_string_lossy());
    for suffix in 1_u64.. {
        let name = match &extension {
            Some(extension) => format!("{stem} ({suffix}).{extension}"),
            None => format!("{stem} ({suffix})"),
        };
        let candidate = downloads_dir.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    unreachable!("the download suffix space is unbounded")
}
pub use initialization::browser_user_input_initialization_script;
