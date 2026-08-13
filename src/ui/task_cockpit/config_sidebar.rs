//! Bounded, read-only projection of the canonical configuration for the
//! Task Cockpit's left configuration rail.
//!
//! This module deliberately does not open `config.json`, mutate `AppConfig`,
//! or invent a provider/service registry.  The host/app supplies the validated
//! [`ConfigSnapshot`] and the shell dispatches the typed selection requests.
//! Configuration editing remains behind the existing ConfigStore/app facade.

use gpui::{
    div, px, rgb, AnyElement, FontWeight, InteractiveElement, IntoElement, ParentElement, Styled,
};

use crate::config::{
    AppConfig, ConfigRevision, ConfigSnapshot, Nullable, Project, ProjectFolder, RunCommand,
    SSHConnection, Settings,
};
use crate::domain::{ConfigSidebarProviderKind, ConfigSidebarSnapshot};
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
            Self::NoConfiguredItems => "No projects yet",
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
    pub workspace_id: String,
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
            .map(|project| {
                project_row(
                    project,
                    config
                        .workspace_project_ids()
                        .get(&project.id)
                        .cloned()
                        .unwrap_or_default(),
                )
            })
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

    /// Project the host-issued redacted snapshot. This path never receives
    /// absolute roots, command text, environment values, or credentials.
    pub fn from_host_snapshot(snapshot: &ConfigSidebarSnapshot) -> Self {
        let projects = snapshot
            .projects
            .iter()
            .take(MAX_CONFIG_PROJECTS)
            .map(|project| ConfigProjectRow {
                config_id: bounded(&project.config_id, MAX_CONFIG_LABEL_SCALARS),
                workspace_id: bounded(&project.workspace_id, MAX_CONFIG_LABEL_SCALARS),
                label: bounded(&project.label, MAX_CONFIG_LABEL_SCALARS),
                root_configured: project.root_configured,
                folders: project
                    .folders
                    .iter()
                    .take(MAX_CONFIG_FOLDERS_PER_PROJECT)
                    .map(|folder| ConfigFolderRow {
                        config_id: bounded(&folder.config_id, MAX_CONFIG_LABEL_SCALARS),
                        label: bounded(&folder.label, MAX_CONFIG_LABEL_SCALARS),
                        server_count: folder.server_count,
                        action: ConfigSidebarAction::enabled(
                            ConfigSidebarActionRequest::SelectFolder {
                                project_id: bounded(&project.config_id, MAX_CONFIG_LABEL_SCALARS),
                                folder_id: bounded(&folder.config_id, MAX_CONFIG_LABEL_SCALARS),
                            },
                        ),
                        accessibility: accessibility(
                            AccessibleRole::Button,
                            &format!(
                                "Folder {}",
                                bounded(&folder.label, MAX_CONFIG_LABEL_SCALARS)
                            ),
                        ),
                    })
                    .collect(),
                action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::SelectProject {
                    config_id: bounded(&project.config_id, MAX_CONFIG_LABEL_SCALARS),
                }),
                accessibility: accessibility(
                    AccessibleRole::Button,
                    &format!(
                        "Project {}",
                        bounded(&project.label, MAX_CONFIG_LABEL_SCALARS)
                    ),
                ),
            })
            .collect::<Vec<_>>();
        let servers: Vec<ConfigServerRow> = snapshot
            .servers
            .iter()
            .take(MAX_CONFIG_SERVERS)
            .map(|server| {
                let project_id = bounded(&server.project_id, MAX_CONFIG_LABEL_SCALARS);
                let folder_id = bounded(&server.folder_id, MAX_CONFIG_LABEL_SCALARS);
                let command_id = bounded(&server.command_id, MAX_CONFIG_LABEL_SCALARS);
                let label = bounded(&server.label, MAX_CONFIG_LABEL_SCALARS);
                ConfigServerRow {
                    project_id: project_id.clone(),
                    folder_id: folder_id.clone(),
                    command_id: command_id.clone(),
                    project_label: bounded(&server.project_label, MAX_CONFIG_LABEL_SCALARS),
                    folder_label: bounded(&server.folder_label, MAX_CONFIG_LABEL_SCALARS),
                    label: label.clone(),
                    port: server.port,
                    action: ConfigSidebarAction::enabled(
                        ConfigSidebarActionRequest::SelectServer {
                            project_id,
                            folder_id,
                            command_id,
                        },
                    ),
                    accessibility: accessibility(
                        AccessibleRole::Button,
                        &format!("Server {label}"),
                    ),
                }
            })
            .collect();
        let ssh_connections: Vec<ConfigSshRow> = snapshot
            .ssh_connections
            .iter()
            .take(MAX_CONFIG_SERVERS)
            .map(|connection| {
                let config_id = bounded(&connection.config_id, MAX_CONFIG_LABEL_SCALARS);
                let label = bounded(&connection.label, MAX_CONFIG_LABEL_SCALARS);
                ConfigSshRow {
                    config_id: config_id.clone(),
                    label: label.clone(),
                    host: bounded(&connection.host, MAX_CONFIG_HOST_SCALARS),
                    port: connection.port,
                    username: bounded(&connection.username, MAX_CONFIG_LABEL_SCALARS),
                    action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::SelectSsh {
                        config_id,
                    }),
                    accessibility: accessibility(
                        AccessibleRole::Button,
                        &format!("Remote {label}"),
                    ),
                }
            })
            .collect();
        let providers = snapshot
            .providers
            .iter()
            .take(MAX_CONFIG_PROVIDERS)
            .map(|provider| ConfigProviderRow {
                provider: match provider.provider {
                    ConfigSidebarProviderKind::Claude => ConfigProvider::Claude,
                    ConfigSidebarProviderKind::Codex => ConfigProvider::Codex,
                },
                label: match provider.provider {
                    ConfigSidebarProviderKind::Claude => ConfigProvider::Claude.label(),
                    ConfigSidebarProviderKind::Codex => ConfigProvider::Codex.label(),
                },
                command_configured: provider.command_configured,
                action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::SelectProvider {
                    provider: match provider.provider {
                        ConfigSidebarProviderKind::Claude => ConfigProvider::Claude,
                        ConfigSidebarProviderKind::Codex => ConfigProvider::Codex,
                    },
                }),
                accessibility: accessibility(
                    AccessibleRole::Button,
                    match provider.provider {
                        ConfigSidebarProviderKind::Claude => ConfigProvider::Claude.label(),
                        ConfigSidebarProviderKind::Codex => ConfigProvider::Codex.label(),
                    },
                ),
            })
            .collect::<Vec<_>>();
        let unavailable_reason = (projects.is_empty()
            && servers.is_empty()
            && ssh_connections.is_empty()
            && providers
                .iter()
                .all(|provider| !provider.command_configured))
        .then_some(ConfigSidebarUnavailableReason::NoConfiguredItems);
        Self {
            revision: Some(snapshot.revision),
            projects,
            servers,
            ssh_connections,
            providers,
            settings_action: ConfigSidebarAction::enabled(ConfigSidebarActionRequest::OpenSettings),
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
                if self.servers.is_empty() && self.ssh_connections.is_empty() {
                    match self.projects.as_slice() {
                        [] => "No projects yet".to_owned(),
                        [project] => project.label.clone(),
                        projects => format!("{} projects", projects.len()),
                    }
                } else {
                    format!(
                        "{} project(s) · {} server(s) · {} remote(s)",
                        self.projects.len(),
                        self.servers.len(),
                        self.ssh_connections.len()
                    )
                }
            },
            |reason| reason.label().to_owned(),
        )
    }

    pub fn project_section_title(&self) -> &'static str {
        "Projects"
    }

    /// One added project already appears in the Projects list. Repeating it under
    /// a Workspace heading makes the same name look like two different things.
    pub fn shows_identity_summary(&self) -> bool {
        matches!(
            self.unavailable_reason,
            Some(ConfigSidebarUnavailableReason::SnapshotMissing)
                | Some(ConfigSidebarUnavailableReason::StoreRecoveryRequired)
        )
    }

    /// Render-only surface. The shell owns dispatch and can therefore preserve
    /// the native terminal/browser surfaces while this rail is updated.
    pub fn surface(
        &self,
        tokens: ThemeTokens,
        projects_heading_action: Option<AnyElement>,
    ) -> AnyElement {
        let mut sections = Vec::new();
        sections.push(section(
            self.project_section_title(),
            self.projects.iter().map(|row| row.label.clone()),
            0,
            tokens,
            projects_heading_action,
        ));
        if !self.servers.is_empty() {
            sections.push(section(
                "Servers",
                self.servers.iter().map(|row| row.label.clone()),
                1,
                tokens,
                None,
            ));
        }
        if !self.ssh_connections.is_empty() {
            sections.push(section(
                "Remote connections",
                self.ssh_connections.iter().map(|row| row.label.clone()),
                3,
                tokens,
                None,
            ));
        }
        div()
            .id("native-config-sidebar")
            // The rail's width is owned by the shell's persisted layout, so the
            // surface fills whatever the user dragged it to instead of pinning
            // a second, conflicting width here.
            .w_full()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(tokens.surfaces.overlay.to_u32()))
            .children(self.shows_identity_summary().then(|| {
                div()
                    .id("native-config-sidebar-summary")
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(tokens.density.spacing.xxs))
                    .px(px(tokens.density.spacing.lg))
                    .py(px(tokens.density.spacing.md))
                    .border_b(px(1.0))
                    .border_color(rgb(tokens.borders.subtle.to_u32()))
                    .child(
                        div()
                            .text_size(px(tokens.density.typography.caption))
                            .line_height(px(tokens.density.typography.caption_line_height))
                            .text_color(rgb(tokens.text.secondary.to_u32()))
                            .child(self.summary()),
                    )
            }))
            .child(
                div()
                    .id("native-config-sidebar-sections")
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .gap(px(tokens.density.spacing.lg))
                    .px(px(tokens.density.spacing.sm))
                    .py(px(tokens.density.spacing.md))
                    .children(sections),
            )
            .into_any_element()
    }
}

fn project_row(project: &Project, workspace_id: String) -> ConfigProjectRow {
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
        workspace_id: bounded(&workspace_id, MAX_CONFIG_LABEL_SCALARS),
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
    heading_action: Option<AnyElement>,
) -> AnyElement {
    let labels: Vec<String> = labels.into_iter().take(MAX_CONFIG_SERVERS).collect();
    let is_empty = labels.is_empty();
    // Row ids are offset by their section so two rails cannot mint the same
    // GPUI element identity for different rows.
    let rows = labels.into_iter().enumerate().map(|(index, label)| {
        div()
            .id((
                "native-config-sidebar-row",
                section_id * MAX_CONFIG_SERVERS + index,
            ))
            .w_full()
            .flex()
            .items_center()
            .gap(px(tokens.density.spacing.sm))
            .h(px(tokens.density.controls.row_height))
            .px(px(tokens.density.spacing.sm))
            .rounded(px(tokens.density.radii.sm))
            .text_size(px(tokens.density.typography.body))
            .line_height(px(tokens.density.typography.body_line_height))
            .text_color(rgb(tokens.text.secondary.to_u32()))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(tokens.surfaces.hover.to_u32())))
            .child(
                div()
                    .flex_none()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded(px(tokens.density.radii.pill))
                    .bg(rgb(tokens.borders.strong.to_u32())),
            )
            .child(div().flex_1().min_w(px(0.0)).truncate().child(label))
    });
    div()
        .id(("native-config-sidebar-section", section_id))
        .w_full()
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.xxs))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .px(px(tokens.density.spacing.sm))
                .pb(px(tokens.density.spacing.xxs))
                .child(
                    div()
                        .text_size(px(tokens.density.typography.caption))
                        .line_height(px(tokens.density.typography.caption_line_height))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(tokens.text.muted.to_u32()))
                        .child(title.to_uppercase()),
                )
                .children(heading_action),
        )
        .children(rows)
        .children(is_empty.then(|| {
            div()
                .flex()
                .items_center()
                .h(px(tokens.density.controls.row_height))
                .px(px(tokens.density.spacing.sm))
                .text_size(px(tokens.density.typography.caption))
                .line_height(px(tokens.density.typography.caption_line_height))
                .text_color(rgb(tokens.text.disabled.to_u32()))
                .child("None configured")
        }))
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
        assert_eq!(projection.summary(), "No projects yet");
    }

    #[test]
    fn summary_names_a_single_project_instead_of_empty_server_counts() {
        let mut config = AppConfig::default();
        config.projects.push(Project {
            id: "project-1".into(),
            name: "Notes".into(),
            root_path: "C:/notes".into(),
            ..Project::default()
        });
        let projection = ConfigSidebarProjection::from_config(&config, 7);
        assert_eq!(projection.summary(), "Notes");
    }

    #[test]
    fn a_single_project_is_listed_once_under_projects() {
        let mut config = AppConfig::default();
        config.projects.push(Project {
            id: "project-1".into(),
            name: "command".into(),
            root_path: "C:/Code/command".into(),
            ..Project::default()
        });
        let projection = ConfigSidebarProjection::from_config(&config, 7);
        assert_eq!(projection.project_section_title(), "Projects");
        assert!(
            !projection.shows_identity_summary(),
            "one folder must not also appear under a Workspace summary"
        );
    }

    #[test]
    fn recovery_still_explains_why_the_rail_is_empty() {
        let projection =
            ConfigSidebarProjection::unavailable(ConfigSidebarUnavailableReason::SnapshotMissing);
        assert!(projection.shows_identity_summary());
    }
}
