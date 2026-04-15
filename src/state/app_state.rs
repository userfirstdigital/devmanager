use crate::models::{
    AppConfig, Project, ProjectFolder, RunCommand, SSHConnection, SessionState, SessionTab,
    Settings, TabType, WindowBoundsState,
};
use crate::persistence::WorkspaceSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn initial_app_state_revision() -> u64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveTerminalSpec {
    pub session_id: String,
    pub cwd: PathBuf,
    pub display_label: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CommandLookup<'a> {
    pub project: &'a Project,
    pub folder: &'a ProjectFolder,
    pub command: &'a RunCommand,
}

#[derive(Debug, Clone, Copy)]
pub struct FolderLookup<'a> {
    pub project: &'a Project,
    pub folder: &'a ProjectFolder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    #[serde(skip, default = "initial_app_state_revision")]
    revision: u64,
    pub config: AppConfig,
    pub open_tabs: Vec<SessionTab>,
    pub active_tab_id: Option<String>,
    pub sidebar_collapsed: bool,
    pub collapsed_projects: BTreeSet<String>,
    pub window_bounds: Option<WindowBoundsState>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::from_workspace(WorkspaceSnapshot::default())
    }
}

impl AppState {
    pub fn from_workspace(snapshot: WorkspaceSnapshot) -> Self {
        let session = snapshot.session.normalize();

        Self {
            revision: initial_app_state_revision(),
            config: snapshot.config.migrate(),
            open_tabs: session.open_tabs,
            active_tab_id: session.active_tab_id,
            sidebar_collapsed: session.sidebar_collapsed,
            collapsed_projects: session.collapsed_projects.into_iter().collect(),
            window_bounds: session.window_bounds,
        }
    }

    pub fn session_state(&self) -> SessionState {
        SessionState {
            open_tabs: self.open_tabs.clone(),
            active_tab_id: self.active_tab_id.clone(),
            sidebar_collapsed: self.sidebar_collapsed,
            collapsed_projects: self.collapsed_projects.iter().cloned().collect(),
            window_bounds: self.window_bounds,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn mark_dirty(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    pub fn toggle_project_collapsed(&mut self, project_id: &str) {
        if !self.collapsed_projects.remove(project_id) {
            self.collapsed_projects.insert(project_id.to_string());
        }
        self.mark_dirty();
    }

    pub fn is_project_collapsed(&self, project_id: &str) -> bool {
        self.collapsed_projects.contains(project_id)
    }

    pub fn projects(&self) -> &[Project] {
        &self.config.projects
    }

    pub fn settings(&self) -> &Settings {
        &self.config.settings
    }

    pub fn ssh_connections(&self) -> &[SSHConnection] {
        &self.config.ssh_connections
    }

    pub fn find_project(&self, project_id: &str) -> Option<&Project> {
        self.config
            .projects
            .iter()
            .find(|project| project.id == project_id)
    }

    pub fn find_command(&self, command_id: &str) -> Option<CommandLookup<'_>> {
        for project in &self.config.projects {
            for folder in &project.folders {
                if let Some(command) = folder.commands.iter().find(|cmd| cmd.id == command_id) {
                    return Some(CommandLookup {
                        project,
                        folder,
                        command,
                    });
                }
            }
        }
        None
    }

    pub fn find_folder(&self, project_id: &str, folder_id: &str) -> Option<FolderLookup<'_>> {
        let project = self.find_project(project_id)?;
        let folder = project
            .folders
            .iter()
            .find(|folder| folder.id == folder_id)?;
        Some(FolderLookup { project, folder })
    }

    pub fn find_ssh_connection(&self, connection_id: &str) -> Option<&SSHConnection> {
        self.config
            .ssh_connections
            .iter()
            .find(|connection| connection.id == connection_id)
    }

    pub fn active_tab(&self) -> Option<&SessionTab> {
        self.active_tab_id
            .as_ref()
            .and_then(|active_id| self.open_tabs.iter().find(|tab| &tab.id == active_id))
    }

    pub fn find_tab(&self, tab_id: &str) -> Option<&SessionTab> {
        self.open_tabs.iter().find(|tab| tab.id == tab_id)
    }

    pub fn find_ai_tab(&self, tab_id: &str) -> Option<&SessionTab> {
        self.find_tab(tab_id)
            .filter(|tab| matches!(tab.tab_type, TabType::Claude | TabType::Codex))
    }

    pub fn find_ai_tab_by_session(&self, session_id: &str) -> Option<&SessionTab> {
        self.ai_tabs()
            .find(|tab| tab.pty_session_id.as_deref() == Some(session_id))
    }

    pub fn find_ssh_tab(&self, tab_id: &str) -> Option<&SessionTab> {
        self.find_tab(tab_id)
            .filter(|tab| matches!(tab.tab_type, TabType::Ssh))
    }

    pub fn find_ssh_tab_by_connection(&self, connection_id: &str) -> Option<&SessionTab> {
        self.ssh_tabs()
            .find(|tab| tab.ssh_connection_id.as_deref() == Some(connection_id))
    }

    pub fn active_project(&self) -> Option<&Project> {
        let project_id = self.active_tab().map(|tab| tab.project_id.as_str())?;

        self.config
            .projects
            .iter()
            .find(|project| project.id == project_id)
    }

    pub fn ai_tabs(&self) -> impl Iterator<Item = &SessionTab> + '_ {
        self.open_tabs
            .iter()
            .filter(|tab| matches!(tab.tab_type, TabType::Claude | TabType::Codex))
    }

    pub fn ssh_tabs(&self) -> impl Iterator<Item = &SessionTab> + '_ {
        self.open_tabs
            .iter()
            .filter(|tab| matches!(tab.tab_type, TabType::Ssh))
    }

    pub fn ai_tabs_for_project(
        &self,
        project_id: &str,
        tab_type: TabType,
    ) -> impl Iterator<Item = &SessionTab> + '_ {
        let project_id = project_id.to_string();
        self.open_tabs.iter().filter(move |tab| {
            tab.project_id == project_id
                && tab.tab_type == tab_type
                && matches!(tab.tab_type, TabType::Claude | TabType::Codex)
        })
    }

    pub fn project_command_count(&self, project: &Project) -> usize {
        project
            .folders
            .iter()
            .map(|folder| folder.commands.len())
            .sum()
    }

    pub fn tab_label(&self, tab: &SessionTab) -> String {
        match tab.tab_type {
            TabType::Server => tab
                .command_id
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| "Server".to_string()),
            TabType::Claude => tab.label.clone().unwrap_or_else(|| "Claude".to_string()),
            TabType::Codex => tab.label.clone().unwrap_or_else(|| "Codex".to_string()),
            TabType::Ssh => tab.label.clone().unwrap_or_else(|| "SSH".to_string()),
        }
    }

    pub fn select_tab(&mut self, tab_id: &str) -> bool {
        if self.open_tabs.iter().any(|tab| tab.id == tab_id) {
            if self.active_tab_id.as_deref() != Some(tab_id) {
                self.active_tab_id = Some(tab_id.to_string());
                self.mark_dirty();
            }
            true
        } else {
            false
        }
    }

    pub fn set_sidebar_collapsed(&mut self, collapsed: bool) {
        if self.sidebar_collapsed != collapsed {
            self.sidebar_collapsed = collapsed;
            self.mark_dirty();
        }
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.mark_dirty();
    }

    pub fn ensure_server_tab(
        &mut self,
        project_id: &str,
        command_id: &str,
        label: Option<String>,
    ) -> String {
        let tab_id = command_id.to_string();
        if let Some(tab) = self.open_tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.project_id = project_id.to_string();
            tab.tab_type = TabType::Server;
            tab.command_id = Some(command_id.to_string());
            tab.pty_session_id = Some(command_id.to_string());
            tab.label = label;
            tab.ssh_connection_id = None;
        } else {
            self.open_tabs.push(SessionTab {
                id: tab_id.clone(),
                tab_type: TabType::Server,
                project_id: project_id.to_string(),
                command_id: Some(command_id.to_string()),
                pty_session_id: Some(command_id.to_string()),
                label,
                ssh_connection_id: None,
            });
        }
        self.mark_dirty();
        tab_id
    }

    pub fn open_server_tab(
        &mut self,
        project_id: &str,
        command_id: &str,
        label: Option<String>,
    ) -> String {
        let tab_id = self.ensure_server_tab(project_id, command_id, label);
        if self.active_tab_id.as_deref() != Some(tab_id.as_str()) {
            self.active_tab_id = Some(tab_id.clone());
            self.mark_dirty();
        }
        tab_id
    }

    pub fn open_ai_tab(
        &mut self,
        project_id: &str,
        tab_type: TabType,
        tab_id: String,
        pty_session_id: String,
        label: Option<String>,
    ) -> String {
        self.open_ai_tab_with_activation(
            project_id,
            tab_type,
            tab_id,
            pty_session_id,
            label,
            true,
        )
    }

    /// Same as `open_ai_tab` but lets callers decide whether this tab
    /// should become the currently-focused UI tab. Remote-triggered
    /// launches pass `activate = false` so a browser creating an AI
    /// session doesn't yank the native window's focus onto a mid-spawn
    /// terminal — the GPUI render of that terminal under a fresh Claude
    /// Code output flood stalls the main thread badly enough to produce
    /// "(Not Responding)".
    pub fn open_ai_tab_with_activation(
        &mut self,
        project_id: &str,
        tab_type: TabType,
        tab_id: String,
        pty_session_id: String,
        label: Option<String>,
        activate: bool,
    ) -> String {
        if !matches!(tab_type, TabType::Claude | TabType::Codex) {
            return tab_id;
        }

        if let Some(tab) = self.open_tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.project_id = project_id.to_string();
            tab.tab_type = tab_type;
            tab.command_id = None;
            tab.pty_session_id = Some(pty_session_id);
            tab.label = label;
            tab.ssh_connection_id = None;
        } else {
            self.open_tabs.push(SessionTab {
                id: tab_id.clone(),
                tab_type,
                project_id: project_id.to_string(),
                command_id: None,
                pty_session_id: Some(pty_session_id),
                label,
                ssh_connection_id: None,
            });
        }

        if activate {
            if self.active_tab_id.as_deref() != Some(tab_id.as_str()) {
                self.active_tab_id = Some(tab_id.clone());
            }
        }
        self.mark_dirty();
        tab_id
    }

    pub fn update_ai_tab_session(&mut self, tab_id: &str, pty_session_id: String) -> bool {
        if let Some(tab) = self.open_tabs.iter_mut().find(|tab| {
            tab.id == tab_id && matches!(tab.tab_type, TabType::Claude | TabType::Codex)
        }) {
            tab.pty_session_id = Some(pty_session_id);
            self.mark_dirty();
            true
        } else {
            false
        }
    }

    pub fn open_ssh_tab(
        &mut self,
        project_id: &str,
        connection_id: &str,
        label: Option<String>,
    ) -> String {
        let tab_id = self
            .find_ssh_tab_by_connection(connection_id)
            .map(|tab| tab.id.clone())
            .unwrap_or_else(|| format!("{connection_id}-tab"));

        if let Some(tab) = self.open_tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.project_id = project_id.to_string();
            tab.tab_type = TabType::Ssh;
            tab.command_id = None;
            tab.label = label.or_else(|| tab.label.clone());
            tab.ssh_connection_id = Some(connection_id.to_string());
        } else {
            self.open_tabs.push(SessionTab {
                id: tab_id.clone(),
                tab_type: TabType::Ssh,
                project_id: project_id.to_string(),
                command_id: None,
                pty_session_id: None,
                label,
                ssh_connection_id: Some(connection_id.to_string()),
            });
        }

        if self.active_tab_id.as_deref() != Some(tab_id.as_str()) {
            self.active_tab_id = Some(tab_id.clone());
        }
        self.mark_dirty();
        tab_id
    }

    pub fn update_ssh_tab_session(&mut self, tab_id: &str, pty_session_id: Option<String>) -> bool {
        if let Some(tab) = self
            .open_tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id && matches!(tab.tab_type, TabType::Ssh))
        {
            tab.pty_session_id = pty_session_id;
            self.mark_dirty();
            true
        } else {
            false
        }
    }

    pub fn remove_tab(&mut self, tab_id: &str) -> bool {
        let original_len = self.open_tabs.len();
        self.open_tabs.retain(|tab| tab.id != tab_id);
        let removed = self.open_tabs.len() != original_len;
        if removed
            && self
                .active_tab_id
                .as_ref()
                .is_some_and(|active| active == tab_id)
        {
            self.active_tab_id = None;
        }
        if removed {
            self.mark_dirty();
        }
        removed
    }

    pub fn merge_recovered_server_tabs(&mut self, recovered_tabs: Vec<SessionTab>) -> usize {
        let mut added = 0;
        let previous_active = self.active_tab_id.clone();
        for tab in recovered_tabs {
            if !self.open_tabs.iter().any(|existing| existing.id == tab.id) {
                self.open_tabs.push(tab);
                added += 1;
            }
        }
        if self
            .active_tab_id
            .as_ref()
            .is_none_or(|active| !self.open_tabs.iter().any(|tab| &tab.id == active))
        {
            self.active_tab_id = self.open_tabs.first().map(|tab| tab.id.clone());
        }
        if added > 0 || previous_active != self.active_tab_id {
            self.mark_dirty();
        }
        added
    }

    pub fn merge_recovered_ai_tabs(&mut self, recovered_tabs: Vec<SessionTab>) -> usize {
        let mut added = 0;
        let previous_active = self.active_tab_id.clone();
        for tab in recovered_tabs {
            if !matches!(tab.tab_type, TabType::Claude | TabType::Codex) {
                continue;
            }
            if !self.open_tabs.iter().any(|existing| existing.id == tab.id) {
                self.open_tabs.push(tab);
                added += 1;
            }
        }
        if self
            .active_tab_id
            .as_ref()
            .is_none_or(|active| !self.open_tabs.iter().any(|tab| &tab.id == active))
        {
            self.active_tab_id = self.open_tabs.first().map(|tab| tab.id.clone());
        }
        if added > 0 || previous_active != self.active_tab_id {
            self.mark_dirty();
        }
        added
    }

    pub fn merge_recovered_ssh_tabs(&mut self, recovered_tabs: Vec<SessionTab>) -> usize {
        let mut added = 0;
        let previous_active = self.active_tab_id.clone();
        for tab in recovered_tabs {
            if !matches!(tab.tab_type, TabType::Ssh) {
                continue;
            }
            if !self.open_tabs.iter().any(|existing| existing.id == tab.id) {
                self.open_tabs.push(tab);
                added += 1;
            }
        }
        if self
            .active_tab_id
            .as_ref()
            .is_none_or(|active| !self.open_tabs.iter().any(|tab| &tab.id == active))
        {
            self.active_tab_id = self.open_tabs.first().map(|tab| tab.id.clone());
        }
        if added > 0 || previous_active != self.active_tab_id {
            self.mark_dirty();
        }
        added
    }

    pub fn next_ai_label(&self, project_id: &str, tab_type: TabType) -> String {
        let next_index = self
            .ai_tabs_for_project(project_id, tab_type.clone())
            .count()
            .saturating_add(1);
        let base = match tab_type {
            TabType::Claude => "Claude",
            TabType::Codex => "Codex",
            _ => "AI",
        };
        format!("{base} {next_index}")
    }

    pub fn update_settings(&mut self, settings: Settings) {
        self.config.settings = settings;
        self.mark_dirty();
    }

    pub fn upsert_project(&mut self, project: Project) -> String {
        if let Some(existing) = self
            .config
            .projects
            .iter_mut()
            .find(|existing| existing.id == project.id)
        {
            *existing = project.clone();
        } else {
            self.config.projects.push(project.clone());
        }
        self.mark_dirty();
        project.id
    }

    pub fn move_project(&mut self, project_id: &str, direction: i32) -> bool {
        let Some(index) = self.config.projects.iter().position(|p| p.id == project_id) else {
            return false;
        };
        let new_index = index as i32 + direction;
        if new_index < 0 || new_index >= self.config.projects.len() as i32 {
            return false;
        }
        self.config.projects.swap(index, new_index as usize);
        self.mark_dirty();
        true
    }

    pub fn remove_project(&mut self, project_id: &str) -> bool {
        let original_len = self.config.projects.len();
        self.config
            .projects
            .retain(|project| project.id != project_id);
        let removed = self.config.projects.len() != original_len;
        if removed {
            self.open_tabs.retain(|tab| tab.project_id != project_id);
            self.repair_active_tab();
            self.mark_dirty();
        }
        removed
    }

    pub fn upsert_folder(&mut self, project_id: &str, folder: ProjectFolder) -> bool {
        let Some(project) = self
            .config
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return false;
        };

        if let Some(existing) = project
            .folders
            .iter_mut()
            .find(|existing| existing.id == folder.id)
        {
            *existing = folder;
        } else {
            project.folders.push(folder);
        }
        self.mark_dirty();
        true
    }

    pub fn remove_folder(&mut self, project_id: &str, folder_id: &str) -> bool {
        let Some(project) = self
            .config
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return false;
        };

        let command_ids: Vec<String> = project
            .folders
            .iter()
            .find(|folder| folder.id == folder_id)
            .map(|folder| {
                folder
                    .commands
                    .iter()
                    .map(|command| command.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let original_len = project.folders.len();
        project.folders.retain(|folder| folder.id != folder_id);
        let removed = project.folders.len() != original_len;
        if removed {
            self.open_tabs.retain(|tab| {
                let is_removed_server_tab = tab.tab_type == TabType::Server
                    && tab
                        .command_id
                        .as_deref()
                        .is_some_and(|command_id| command_ids.iter().any(|id| id == command_id));
                !is_removed_server_tab
            });
            self.repair_active_tab();
            self.mark_dirty();
        }
        removed
    }

    pub fn upsert_command(
        &mut self,
        project_id: &str,
        folder_id: &str,
        command: RunCommand,
    ) -> bool {
        let Some(project) = self
            .config
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return false;
        };
        let Some(folder) = project
            .folders
            .iter_mut()
            .find(|folder| folder.id == folder_id)
        else {
            return false;
        };

        if let Some(existing) = folder
            .commands
            .iter_mut()
            .find(|existing| existing.id == command.id)
        {
            *existing = command;
        } else {
            folder.commands.push(command);
        }
        self.mark_dirty();
        true
    }

    pub fn remove_command(&mut self, project_id: &str, folder_id: &str, command_id: &str) -> bool {
        let Some(project) = self
            .config
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return false;
        };
        let Some(folder) = project
            .folders
            .iter_mut()
            .find(|folder| folder.id == folder_id)
        else {
            return false;
        };

        let original_len = folder.commands.len();
        folder.commands.retain(|command| command.id != command_id);
        let removed = folder.commands.len() != original_len;
        if removed {
            self.open_tabs.retain(|tab| {
                !(tab.tab_type == TabType::Server && tab.command_id.as_deref() == Some(command_id))
            });
            self.repair_active_tab();
            self.mark_dirty();
        }
        removed
    }

    pub fn upsert_ssh_connection(&mut self, connection: SSHConnection) -> String {
        if let Some(existing) = self
            .config
            .ssh_connections
            .iter_mut()
            .find(|existing| existing.id == connection.id)
        {
            *existing = connection.clone();
        } else {
            self.config.ssh_connections.push(connection.clone());
        }
        self.mark_dirty();
        connection.id
    }

    pub fn remove_ssh_connection(&mut self, connection_id: &str) -> bool {
        let original_len = self.config.ssh_connections.len();
        self.config
            .ssh_connections
            .retain(|connection| connection.id != connection_id);
        let removed = self.config.ssh_connections.len() != original_len;
        if removed {
            self.open_tabs.retain(|tab| {
                !(tab.tab_type == TabType::Ssh
                    && tab.ssh_connection_id.as_deref() == Some(connection_id))
            });
            self.repair_active_tab();
            self.mark_dirty();
        }
        removed
    }

    pub fn active_terminal_spec(&self) -> ActiveTerminalSpec {
        if let Some(tab) = self.active_tab() {
            if matches!(tab.tab_type, TabType::Server) {
                if let Some(command_id) = tab.command_id.as_deref() {
                    if let Some(lookup) = self.find_command(command_id) {
                        let cwd = PathBuf::from(lookup.folder.folder_path.clone());
                        let cwd = if cwd.is_dir() { cwd } else { fallback_cwd() };
                        return ActiveTerminalSpec {
                            session_id: command_id.to_string(),
                            cwd,
                            display_label: lookup.command.label.clone(),
                        };
                    }
                }
            }

            let cwd = self
                .active_project()
                .map(|project| PathBuf::from(project.root_path.clone()))
                .filter(|path| path.is_dir())
                .unwrap_or_else(fallback_cwd);

            return ActiveTerminalSpec {
                session_id: tab
                    .pty_session_id
                    .clone()
                    .or_else(|| tab.command_id.clone())
                    .unwrap_or_else(|| tab.id.clone()),
                cwd,
                display_label: self.tab_label(tab),
            };
        }

        if let Some(tab) = self
            .open_tabs
            .iter()
            .find(|tab| tab.tab_type == TabType::Server)
        {
            if let Some(command_id) = tab.command_id.as_deref() {
                if let Some(lookup) = self.find_command(command_id) {
                    let cwd = PathBuf::from(lookup.folder.folder_path.clone());
                    let cwd = if cwd.is_dir() { cwd } else { fallback_cwd() };
                    return ActiveTerminalSpec {
                        session_id: command_id.to_string(),
                        cwd,
                        display_label: lookup.folder.name.clone(),
                    };
                }
            }
        }

        if let Some(project) = self.config.projects.first() {
            let cwd = PathBuf::from(project.root_path.clone());
            return ActiveTerminalSpec {
                session_id: scoped_shell_session_id(format!("phase1-shell-{}", project.id)),
                cwd: if cwd.is_dir() { cwd } else { fallback_cwd() },
                display_label: project.name.clone(),
            };
        }

        ActiveTerminalSpec {
            session_id: scoped_shell_session_id("phase1-shell"),
            cwd: fallback_cwd(),
            display_label: "Shell".to_string(),
        }
    }
}

fn fallback_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn scoped_shell_session_id(base: impl AsRef<str>) -> String {
    format!(
        "{}-{}",
        base.as_ref(),
        crate::persistence::runtime_session_scope()
    )
}

impl AppState {
    fn repair_active_tab(&mut self) {
        if self
            .active_tab_id
            .as_ref()
            .is_none_or(|active| !self.open_tabs.iter().any(|tab| &tab.id == active))
        {
            self.active_tab_id = self.open_tabs.first().map(|tab| tab.id.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_shell_tab() -> SessionTab {
        SessionTab {
            id: "shell-tab".to_string(),
            tab_type: TabType::Claude,
            project_id: "project-1".to_string(),
            command_id: None,
            pty_session_id: Some("claude-session".to_string()),
            label: Some("Claude".to_string()),
            ssh_connection_id: None,
        }
    }

    #[test]
    fn ensure_server_tab_does_not_change_active_selection() {
        let mut state = AppState::default();
        state.open_tabs.push(sample_shell_tab());
        state.active_tab_id = Some("shell-tab".to_string());

        let tab_id = state.ensure_server_tab("project-1", "server-cmd", Some("Web".to_string()));

        assert_eq!(tab_id, "server-cmd");
        assert_eq!(state.active_tab_id.as_deref(), Some("shell-tab"));
        let tab = state.find_tab("server-cmd").expect("server tab");
        assert_eq!(tab.project_id, "project-1");
        assert_eq!(tab.command_id.as_deref(), Some("server-cmd"));
        assert_eq!(tab.pty_session_id.as_deref(), Some("server-cmd"));
    }

    #[test]
    fn fallback_shell_session_id_is_scoped_to_runtime_instance() {
        let mut state = AppState::default();
        state.config.projects.push(Project {
            id: "project-1".to_string(),
            name: "Project".to_string(),
            root_path: ".".to_string(),
            folders: Vec::new(),
            color: None,
            pinned: Some(false),
            notes: None,
            save_log_files: Some(false),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        });

        let spec = state.active_terminal_spec();

        assert!(spec.session_id.starts_with("phase1-shell-project-1-"));
    }

    #[test]
    fn mutating_state_bumps_revision_but_missing_selection_does_not() {
        let mut state = AppState::default();
        let initial = state.revision();

        state.toggle_sidebar();
        let after_toggle = state.revision();
        assert!(after_toggle > initial);

        assert!(!state.select_tab("missing-tab"));
        assert_eq!(state.revision(), after_toggle);

        state.ensure_server_tab("project-1", "server-cmd", Some("Web".to_string()));
        assert!(state.revision() > after_toggle);
    }
}
