use crate::models::{
    AppConfig, Project, ProjectFolder, RunCommand, SSHConnection, SessionState, SessionTab,
    Settings, TabType,
};
use crate::persistence::WorkspaceSnapshot;
use std::path::PathBuf;

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub open_tabs: Vec<SessionTab>,
    pub active_tab_id: Option<String>,
    pub sidebar_collapsed: bool,
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
            config: snapshot.config.migrate(),
            open_tabs: session.open_tabs,
            active_tab_id: session.active_tab_id,
            sidebar_collapsed: session.sidebar_collapsed,
        }
    }

    pub fn session_state(&self) -> SessionState {
        SessionState {
            open_tabs: self.open_tabs.clone(),
            active_tab_id: self.active_tab_id.clone(),
            sidebar_collapsed: self.sidebar_collapsed,
        }
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
        let Some(project_id) = self.active_tab().map(|tab| tab.project_id.as_str()) else {
            return self.config.projects.first();
        };

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
            self.active_tab_id = Some(tab_id.to_string());
            true
        } else {
            false
        }
    }

    pub fn set_sidebar_collapsed(&mut self, collapsed: bool) {
        self.sidebar_collapsed = collapsed;
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
    }

    pub fn open_server_tab(
        &mut self,
        project_id: &str,
        command_id: &str,
        label: Option<String>,
    ) -> String {
        let tab_id = command_id.to_string();
        if !self.open_tabs.iter().any(|tab| tab.id == tab_id) {
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
        self.active_tab_id = Some(tab_id.clone());
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

        self.active_tab_id = Some(tab_id.clone());
        tab_id
    }

    pub fn update_ai_tab_session(&mut self, tab_id: &str, pty_session_id: String) -> bool {
        if let Some(tab) = self.open_tabs.iter_mut().find(|tab| {
            tab.id == tab_id && matches!(tab.tab_type, TabType::Claude | TabType::Codex)
        }) {
            tab.pty_session_id = Some(pty_session_id);
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

        self.active_tab_id = Some(tab_id.clone());
        tab_id
    }

    pub fn update_ssh_tab_session(&mut self, tab_id: &str, pty_session_id: Option<String>) -> bool {
        if let Some(tab) = self
            .open_tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id && matches!(tab.tab_type, TabType::Ssh))
        {
            tab.pty_session_id = pty_session_id;
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
            self.active_tab_id = self.open_tabs.first().map(|tab| tab.id.clone());
        }
        removed
    }

    pub fn merge_recovered_server_tabs(&mut self, recovered_tabs: Vec<SessionTab>) -> usize {
        let mut added = 0;
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
        added
    }

    pub fn merge_recovered_ai_tabs(&mut self, recovered_tabs: Vec<SessionTab>) -> usize {
        let mut added = 0;
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
        added
    }

    pub fn merge_recovered_ssh_tabs(&mut self, recovered_tabs: Vec<SessionTab>) -> usize {
        let mut added = 0;
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
        project.id
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
                        display_label: lookup.command.label.clone(),
                    };
                }
            }
        }

        if let Some(project) = self.config.projects.first() {
            let cwd = PathBuf::from(project.root_path.clone());
            return ActiveTerminalSpec {
                session_id: format!("phase1-shell-{}", project.id),
                cwd: if cwd.is_dir() { cwd } else { fallback_cwd() },
                display_label: project.name.clone(),
            };
        }

        ActiveTerminalSpec {
            session_id: "phase1-shell".to_string(),
            cwd: fallback_cwd(),
            display_label: "Shell".to_string(),
        }
    }
}

fn fallback_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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
