//! Bounded, read-only projection of the canonical configuration for the
//! Task Cockpit's left configuration rail.
//!
//! This module deliberately does not open `config.json`, mutate `AppConfig`,
//! or invent a provider/service registry.  The host/app supplies the validated
//! [`ConfigSnapshot`] and the shell dispatches the typed selection requests.
//! Configuration editing remains behind the existing ConfigStore/app facade.

use gpui::{div, px, rgb, AnyElement, InteractiveElement, IntoElement, ParentElement, Styled};

use crate::config::{
    AppConfig, ConfigRevision, ConfigSnapshot, Nullable, Project, ProjectFolder, RunCommand,
    SSHConnection, Settings,
};
use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};
use crate::ui::tokens::ThemeTokens;

pub const MAX_CONFIG_PROJECTS: usize = 128;
pub const MAX_CONFIG_FOLDERS_PER_PROJECT: usize = 64;
pub const MAX_CONFIG_SERVERS: usize = 256;
pub const MAX_CONFIG_PROVIDERS: usize = 8;
pub const MAX_CONFIG_LABEL_SCALARS: usize = 96;
pub const MAX_CONFIG_HOST_SCALARS: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSidebarUnavailableReason {
    SnapshotMissing,
    StoreRecoveryRequired,
    NoConfiguredItems,
}

impl ConfigSidebarUnavailableReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::SnapshotMissing => "Configuration is unavailable",
            Self::StoreRecoveryRequired => "Configuration is read-only until recovery succeeds",
            Self::NoConfiguredItems => "No configured projects, servers, or providers",
        }
    }
}

/// Selection-only requests emitted by the projection.  These are intentionally
/// not config mutations: the native shell maps them to its existing sidebar
/// navigation/config editor boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSidebarActionRequest {
    SelectProject {
        config_id: String,
    },
    SelectFolder {
        project_id: String,
        folder_id: String,
    },
    SelectServer {
        project_id: String,
        folder_id: String,
        command_id: String,
    },
    SelectSsh {
        config_id: String,
    },
    SelectProvider {
        provider: ConfigProvider,
    },
    OpenSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigProvider {
    Claude,
    Codex,
}

impl ConfigProvider {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSidebarDisabledReason {
    ReadOnly,
    Archived,
    MissingConfigId,
}

impl ConfigSidebarDisabledReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Configuration editing is unavailable",
            Self::Archived => "Archived configuration is not selectable",
            Self::MissingConfigId => "Configuration identity is unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSidebarAction {
    pub request: ConfigSidebarActionRequest,
    pub disabled_reason: Option<ConfigSidebarDisabledReason>,
}

impl ConfigSidebarAction {
    fn enabled(request: ConfigSidebarActionRequest) -> Self {
        Self {
            request,
            disabled_reason: None,
        }
    }

    pub const fn is_enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProjectRow {
    pub config_id: String,
    pub label: String,
    pub root_configured: bool,
    pub folders: Vec<ConfigFolderRow>,
    pub action: ConfigSidebarAction,
    pub accessibility: AccessibilityMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFolderRow {
    pub config_id: String,
    pub label: String,
    pub server_count: usize,
    pub action: ConfigSidebarAction,
    pub accessibility: AccessibilityMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigServerRow {
    pub project_id: String,
    pub folder_id: String,
    pub command_id: String,
    pub project_label: String,
    pub folder_label: String,
    pub label: String,
    pub port: Option<u16>,
    pub action: ConfigSidebarAction,
    pub accessibility: AccessibilityMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSshRow {
    pub config_id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub action: ConfigSidebarAction,
    pub accessibility: AccessibilityMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProviderRow {
    pub provider: ConfigProvider,
    pub label: &'static str,
    pub command_configured: bool,
    pub action: ConfigSidebarAction,
    pub accessibility: AccessibilityMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSidebarProjection {
    pub revision: Option<ConfigRevision>,
    pub projects: Vec<ConfigProjectRow>,
    pub servers: Vec<ConfigServerRow>,
    pub ssh_connections: Vec<ConfigSshRow>,
    pub providers: Vec<ConfigProviderRow>,
    pub settings_action: ConfigSidebarAction,
    pub unavailable_reason: Option<ConfigSidebarUnavailableReason>,
}

impl ConfigSidebarProjection {
    /// Project the validated host snapshot. A missing snapshot is represented
    /// explicitly; a default `AppConfig` is never synthesized for the UI.
    pub fn from_snapshot(snapshot: Option<&ConfigSnapshot>) -> Self {
        match snapshot {
            Some(snapshot) => Self::from_config(&snapshot.config, snapshot.revision),
            None => Self::unavailable(ConfigSidebarUnavailableReason::SnapshotMissing),
        }
    }

    /// Project directly from the existing authoritative config facade.
    pub fn from_config(config: &AppConfig, revision: ConfigRevision) -> Self {
        let projects = config
            .projects
            .iter()
            .filter(|project| !is_archived(&project.archived))
            .take(MAX_CONFIG_PROJECTS)
            .map(project_row)
            .collect::<Vec<_>>();
        let mut servers = Vec::new();
        for project in config
            .projects
            .iter()
            .filter(|project| !is_archived(&project.archived))
        {
            for folder in project
                .folders
                .iter()
                .filter(|folder| !is_archived(&folder.archived))
                .take(MAX_CONFIG_FOLDERS_PER_PROJECT)
            {
                for command in folder
                    .commands
                    .iter()
                    .filter(|command| !is_archived(&command.archived))
                {
                    if servers.len() == MAX_CONFIG_SERVERS {
                        break;
                    }
                    servers.push(server_row(project, folder, command));
                }
            }
        }
        let ssh_connections = config
            .ssh_connections
            .iter()
            .filter(|connection| !is_archived(&connection.archived))
            .take(MAX_CONFIG_SERVERS)
            .map(ssh_row)
            .collect::<Vec<_>>();
        let providers = provider_rows(config.settings());
        let settings_action =
            ConfigSidebarAction::enabled(ConfigSidebarActionRequest::OpenSettings);
        let unavailable_reason = (projects.is_empty()
            && servers.is_empty()
            && ssh_connections.is_empty()
            && providers
                .iter()
                .all(|provider| !provider.command_configured))
        .then_some(ConfigSidebarUnavailableReason::NoConfiguredItems);
        Self {
            revision: Some(revision),
            projects,
            servers,
            ssh_connections,
            providers,
            settings_action,
            unavailable_reason,
        }
    }

    pub fn unavailable(reason: ConfigSidebarUnavailableReason) -> Self {
        let settings_action = ConfigSidebarAction {
            request: ConfigSidebarActionRequest::OpenSettings,
            disabled_reason: Some(ConfigSidebarDisabledReason::ReadOnly),
        };
        Self {
            revision: None,
            projects: Vec::new(),
            servers: Vec::new(),
            ssh_connections: Vec::new(),
            providers: Vec::new(),
            settings_action,
            unavailable_reason: Some(reason),
        }
    }

    pub fn summary(&self) -> String {
        self.unavailable_reason.map_or_else(
            || {
                format!(
                    "{} project(s) · {} server(s) · {} remote(s)",
                    self.projects.len(),
                    self.servers.len(),
                    self.ssh_connections.len()
                )
            },
            |reason| reason.label().to_owned(),
        )
    }

    /// Render-only surface. The shell owns dispatch and can therefore preserve
    /// the native terminal/browser surfaces while this rail is updated.
    pub fn surface(&self, tokens: ThemeTokens) -> AnyElement {
        let mut sections = Vec::new();
        sections.push(section(
            "Projects",
            self.projects.iter().map(|row| row.label.clone()),
            0,
            tokens,
        ));
        sections.push(section(
            "Servers",
            self.servers.iter().map(|row| row.label.clone()),
            1,
            tokens,
        ));
        sections.push(section(
            "LLM providers",
            self.providers.iter().map(|row| {
                format!(
                    "{}{}",
                    row.label,
                    if row.command_configured {
                        ""
                    } else {
                        " · not configured"
                    }
                )
            }),
            2,
            tokens,
        ));
        sections.push(section(
            "Remote connections",
            self.ssh_connections.iter().map(|row| row.label.clone()),
            3,
            tokens,
        ));
        div()
            .id("native-config-sidebar")
            .w(px(280.0))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(tokens.density.spacing.sm))
            .p(px(tokens.density.physical().control_padding as f32))
            .bg(rgb(tokens.surfaces.sunken.to_u32()))
            .child(self.summary())
            .children(sections)
            .into_any_element()
    }
}

fn project_row(project: &Project) -> ConfigProjectRow {
    let config_id = bounded(&project.id, MAX_CONFIG_LABEL_SCALARS);
    let label = bounded(
        if project.name.trim().is_empty() {
            &project.id
        } else {
            &project.name
        },
        MAX_CONFIG_LABEL_SCALARS,
    );
    let folders = project
        .folders
        .iter()
        .filter(|folder| !is_archived(&folder.archived))
        .take(MAX_CONFIG_FOLDERS_PER_PROJECT)
        .map(|folder| folder_row(project, folder))
        .collect();
    let accessibility = accessibility(AccessibleRole::Button, &format!("Project {label}"));
    ConfigProjectRow {
        config_id: config_id.clone(),
        label,
        root_configured: !project.root_path.trim().is_empty(),
        folders,
        action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::SelectProject {
            config_id,
        }),
        accessibility,
    }
}

fn folder_row(project: &Project, folder: &ProjectFolder) -> ConfigFolderRow {
    let config_id = bounded(&folder.id, MAX_CONFIG_LABEL_SCALARS);
    let label = bounded(
        if folder.name.trim().is_empty() {
            &folder.id
        } else {
            &folder.name
        },
        MAX_CONFIG_LABEL_SCALARS,
    );
    let server_count = folder
        .commands
        .iter()
        .filter(|command| !is_archived(&command.archived))
        .count();
    let accessibility = accessibility(AccessibleRole::Button, &format!("Folder {label}"));
    ConfigFolderRow {
        config_id: config_id.clone(),
        label,
        server_count,
        action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::SelectFolder {
            project_id: bounded(&project.id, MAX_CONFIG_LABEL_SCALARS),
            folder_id: config_id,
        }),
        accessibility,
    }
}

fn server_row(project: &Project, folder: &ProjectFolder, command: &RunCommand) -> ConfigServerRow {
    let project_id = bounded(&project.id, MAX_CONFIG_LABEL_SCALARS);
    let folder_id = bounded(&folder.id, MAX_CONFIG_LABEL_SCALARS);
    let command_id = bounded(&command.id, MAX_CONFIG_LABEL_SCALARS);
    let label = bounded(
        if command.label.trim().is_empty() {
            &command.id
        } else {
            &command.label
        },
        MAX_CONFIG_LABEL_SCALARS,
    );
    let accessibility = accessibility(AccessibleRole::Button, &format!("Server {label}"));
    ConfigServerRow {
        project_id: project_id.clone(),
        folder_id: folder_id.clone(),
        command_id: command_id.clone(),
        project_label: bounded(&project.name, MAX_CONFIG_LABEL_SCALARS),
        folder_label: bounded(&folder.name, MAX_CONFIG_LABEL_SCALARS),
        label,
        port: command.port.as_ref().copied(),
        action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::SelectServer {
            project_id,
            folder_id,
            command_id,
        }),
        accessibility,
    }
}

fn ssh_row(connection: &SSHConnection) -> ConfigSshRow {
    let config_id = bounded(&connection.id, MAX_CONFIG_LABEL_SCALARS);
    let label = bounded(
        if connection.label.trim().is_empty() {
            &connection.id
        } else {
            &connection.label
        },
        MAX_CONFIG_LABEL_SCALARS,
    );
    let host = bounded(&connection.host, MAX_CONFIG_HOST_SCALARS);
    let username = bounded(&connection.username, MAX_CONFIG_LABEL_SCALARS);
    let accessibility = accessibility(AccessibleRole::Button, &format!("Remote {label}"));
    ConfigSshRow {
        config_id: config_id.clone(),
        label,
        host,
        port: connection.port,
        username,
        action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::SelectSsh { config_id }),
        accessibility,
    }
}

fn provider_rows(settings: &Settings) -> Vec<ConfigProviderRow> {
    [
        (ConfigProvider::Claude, settings.claude_command.as_ref()),
        (ConfigProvider::Codex, settings.codex_command.as_ref()),
    ]
    .into_iter()
    .take(MAX_CONFIG_PROVIDERS)
    .map(|(provider, command)| ConfigProviderRow {
        provider,
        label: provider.label(),
        command_configured: command.is_some_and(|value| !value.trim().is_empty()),
        action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::SelectProvider {
            provider,
        }),
        accessibility: accessibility(AccessibleRole::Button, provider.label()),
    })
    .collect()
}

fn is_archived(value: &Nullable<bool>) -> bool {
    value.as_ref().copied().unwrap_or(false)
}

fn bounded(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(max)
        .collect()
}

fn accessibility(role: AccessibleRole, name: &str) -> AccessibilityMetadata {
    AccessibilityMetadata::new(role, name).expect("bounded config sidebar accessibility label")
}

fn section(
    title: &'static str,
    labels: impl IntoIterator<Item = String>,
    section_id: usize,
    tokens: ThemeTokens,
) -> AnyElement {
    let rows = labels
        .into_iter()
        .take(MAX_CONFIG_SERVERS)
        .enumerate()
        .map(|(index, label)| div().id(("native-config-sidebar-row", index)).child(label));
    div()
        .id(("native-config-sidebar-section", section_id))
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.xs))
        .child(title)
        .children(rows)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Project, ProjectFolder, RunCommand};

    fn project() -> Project {
        Project {
            id: "project-1".into(),
            name: "Workspace".into(),
            root_path: "C:/workspace".into(),
            folders: vec![ProjectFolder {
                id: "folder-1".into(),
                name: "Local".into(),
                commands: vec![RunCommand {
                    id: "api".into(),
                    label: "API".into(),
                    port: Nullable::Value(3000),
                    ..RunCommand::default()
                }],
                ..ProjectFolder::default()
            }],
            ..Project::default()
        }
    }

    #[test]
    fn projection_uses_canonical_config_and_emits_exact_selection_requests() {
        let mut config = AppConfig::default();
        config.projects.push(project());
        let projection = ConfigSidebarProjection::from_config(&config, 7);
        assert_eq!(projection.revision, Some(7));
        assert_eq!(projection.projects[0].config_id, "project-1");
        assert!(matches!(
            projection.projects[0].action.request,
            ConfigSidebarActionRequest::SelectProject { ref config_id } if config_id == "project-1"
        ));
        assert_eq!(projection.servers.len(), 1);
        assert_eq!(projection.servers[0].port, Some(3000));
        assert!(matches!(
            projection.servers[0].action.request,
            ConfigSidebarActionRequest::SelectServer { ref project_id, ref folder_id, ref command_id }
                if project_id == "project-1" && folder_id == "folder-1" && command_id == "api"
        ));
    }

    #[test]
    fn projection_filters_archived_rows_and_does_not_fabricate_missing_snapshot() {
        let mut config = AppConfig::default();
        let mut archived = project();
        archived.archived = Nullable::Value(true);
        config.projects.push(archived);
        let projection = ConfigSidebarProjection::from_config(&config, 1);
        assert!(projection.projects.is_empty());
        assert_eq!(
            projection.unavailable_reason,
            Some(ConfigSidebarUnavailableReason::NoConfiguredItems)
        );
        let missing = ConfigSidebarProjection::from_snapshot(None);
        assert_eq!(missing.revision, None);
        assert_eq!(
            missing.unavailable_reason,
            Some(ConfigSidebarUnavailableReason::SnapshotMissing)
        );
        assert!(!missing.settings_action.is_enabled());
    }

    #[test]
    fn provider_projection_is_truthful_about_unconfigured_commands() {
        let projection = ConfigSidebarProjection::from_config(&AppConfig::default(), 2);
        assert_eq!(projection.providers.len(), 2);
        assert!(projection
            .providers
            .iter()
            .all(|provider| !provider.command_configured));
        assert_eq!(
            projection.summary(),
            "No configured projects, servers, or providers"
        );
    }
}
