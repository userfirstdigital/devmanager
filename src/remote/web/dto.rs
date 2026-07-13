use crate::models::{PortStatus, TabType};
use crate::state::{
    AppState, RuntimeState, SessionDimensions, SessionKind, SessionRuntimeState, SessionStatus,
};
use serde::Serialize;
use std::collections::HashMap;

pub const WEB_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebWorkspaceSnapshot {
    pub web_protocol_version: u32,
    pub runtime_instance_id: String,
    pub revision: u64,
    pub server_id: String,
    pub projects: Vec<WebProject>,
    pub ssh_connections: Vec<WebSshConnection>,
    pub tabs: Vec<WebTab>,
    pub sessions: Vec<WebSessionSummary>,
    pub port_statuses: Vec<WebPortStatus>,
    pub writer_lease: WebWriterLeaseState,
}

/// State changes currently send the same complete, allowlisted projection as
/// initial snapshots. This alias keeps the wire boundary web-specific while
/// leaving room for a measured partial-delta representation later.
pub type WebWorkspaceDelta = WebWorkspaceSnapshot;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebWriterLeaseState {
    pub owner_client_instance_id: Option<String>,
    pub generation: u64,
    pub expires_at_epoch_ms: Option<u64>,
    pub you_are_owner: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebProject {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub folders: Vec<WebProjectFolder>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebProjectFolder {
    pub id: String,
    pub name: String,
    pub commands: Vec<WebProjectCommand>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebProjectCommand {
    pub id: String,
    pub label: String,
    pub port: Option<u16>,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSshConnection {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebTabKind {
    Server,
    Claude,
    Codex,
    Ssh,
}

impl From<&TabType> for WebTabKind {
    fn from(value: &TabType) -> Self {
        match value {
            TabType::Server => Self::Server,
            TabType::Claude => Self::Claude,
            TabType::Codex => Self::Codex,
            TabType::Ssh => Self::Ssh,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebTab {
    pub id: String,
    pub kind: WebTabKind,
    pub project_id: String,
    pub command_id: Option<String>,
    pub session_id: Option<String>,
    pub connection_id: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSessionKind {
    Shell,
    Server,
    Claude,
    Codex,
    Ssh,
}

impl From<SessionKind> for WebSessionKind {
    fn from(value: SessionKind) -> Self {
        match value {
            SessionKind::Shell => Self::Shell,
            SessionKind::Server => Self::Server,
            SessionKind::Claude => Self::Claude,
            SessionKind::Codex => Self::Codex,
            SessionKind::Ssh => Self::Ssh,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSessionSummary {
    pub session_id: String,
    pub kind: WebSessionKind,
    pub status: SessionStatus,
    pub project_id: Option<String>,
    pub command_id: Option<String>,
    pub tab_id: Option<String>,
    pub dimensions: SessionDimensions,
}

impl From<&SessionRuntimeState> for WebSessionSummary {
    fn from(session: &SessionRuntimeState) -> Self {
        Self {
            session_id: session.session_id.clone(),
            kind: session.session_kind.into(),
            status: session.status,
            project_id: session.project_id.clone(),
            command_id: session.command_id.clone(),
            tab_id: session.tab_id.clone(),
            dimensions: session.dimensions,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebPortStatus {
    pub port: u16,
    pub in_use: bool,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
}

impl From<&PortStatus> for WebPortStatus {
    fn from(status: &PortStatus) -> Self {
        Self {
            port: status.port,
            in_use: status.in_use,
            pid: status.pid,
            process_name: status.process_name.clone(),
        }
    }
}

impl WebWorkspaceSnapshot {
    pub fn from_host(
        runtime_instance_id: impl Into<String>,
        revision: u64,
        app: &AppState,
        runtime: &RuntimeState,
        ports: &HashMap<u16, PortStatus>,
        lease: &WebWriterLeaseState,
    ) -> Self {
        let projects = app
            .config
            .projects
            .iter()
            .map(|project| WebProject {
                id: project.id.clone(),
                name: project.name.clone(),
                color: project.color.clone(),
                folders: project
                    .folders
                    .iter()
                    .map(|folder| WebProjectFolder {
                        id: folder.id.clone(),
                        name: folder.name.clone(),
                        commands: folder
                            .commands
                            .iter()
                            .map(|command| WebProjectCommand {
                                id: command.id.clone(),
                                label: command.label.clone(),
                                port: command.port,
                                status: runtime
                                    .sessions
                                    .get(&command.id)
                                    .or_else(|| {
                                        runtime.sessions.values().find(|session| {
                                            session.command_id.as_deref()
                                                == Some(command.id.as_str())
                                        })
                                    })
                                    .map(|session| session.status)
                                    .unwrap_or(SessionStatus::Stopped),
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect();

        let ssh_connections = app
            .config
            .ssh_connections
            .iter()
            .map(|connection| WebSshConnection {
                id: connection.id.clone(),
                label: connection.label.clone(),
                host: connection.host.clone(),
                port: connection.port,
                username: connection.username.clone(),
            })
            .collect();

        let tabs = app
            .open_tabs
            .iter()
            .map(|tab| WebTab {
                id: tab.id.clone(),
                kind: (&tab.tab_type).into(),
                project_id: tab.project_id.clone(),
                command_id: tab.command_id.clone(),
                session_id: tab.pty_session_id.clone().or_else(|| {
                    matches!(tab.tab_type, TabType::Server)
                        .then(|| tab.command_id.clone())
                        .flatten()
                }),
                connection_id: tab.ssh_connection_id.clone(),
                label: tab.label.clone(),
            })
            .collect();

        let mut sessions = runtime
            .sessions
            .values()
            .map(WebSessionSummary::from)
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));

        let mut port_statuses = ports.values().map(WebPortStatus::from).collect::<Vec<_>>();
        port_statuses.sort_by_key(|status| status.port);

        Self {
            web_protocol_version: WEB_PROTOCOL_VERSION,
            runtime_instance_id: runtime_instance_id.into(),
            revision,
            server_id: String::new(),
            projects,
            ssh_connections,
            tabs,
            sessions,
            port_statuses,
            writer_lease: lease.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        PortStatus, Project, ProjectFolder, RunCommand, SSHConnection, SessionTab, TabType,
    };
    use crate::state::{
        AiLaunchSpec, AppState, RuntimeState, SessionDimensions, SessionKind, SessionRuntimeState,
    };
    use crate::terminal::session::TerminalBackend;
    use std::collections::HashMap;
    use std::path::PathBuf;

    struct HostFixture {
        app: AppState,
        runtime: RuntimeState,
        ports: HashMap<u16, PortStatus>,
        lease: WebWriterLeaseState,
    }

    fn host_fixture_with_sentinels() -> HostFixture {
        let mut app = AppState::default();
        app.config.projects.push(Project {
            id: "project-1".to_string(),
            name: "Project".to_string(),
            notes: Some("NOTES_SENTINEL".to_string()),
            folders: vec![ProjectFolder {
                id: "folder-1".to_string(),
                name: "Folder".to_string(),
                commands: vec![RunCommand {
                    id: "command-1".to_string(),
                    label: "Server".to_string(),
                    command: "secret executable".to_string(),
                    args: vec!["secret argument".to_string()],
                    env: Some(HashMap::from([(
                        "SECRET".to_string(),
                        "ENV_SENTINEL".to_string(),
                    )])),
                    port: Some(43872),
                    ..RunCommand::default()
                }],
                ..ProjectFolder::default()
            }],
            ..Project::default()
        });
        app.config.ssh_connections.push(SSHConnection {
            id: "ssh-1".to_string(),
            label: "SSH".to_string(),
            host: "example.test".to_string(),
            port: 22,
            username: "dev".to_string(),
            password: Some("PASSWORD_SENTINEL".to_string()),
            private_key: Some("PRIVATE_KEY_SENTINEL".to_string()),
        });
        app.config.settings.github_token = Some("TOKEN_SENTINEL".to_string());
        app.open_tabs.push(SessionTab {
            id: "tab-1".to_string(),
            tab_type: TabType::Claude,
            project_id: "project-1".to_string(),
            pty_session_id: Some("session-1".to_string()),
            ..SessionTab::default()
        });

        let mut runtime = RuntimeState::default();
        let mut session = SessionRuntimeState::new(
            "session-1",
            PathBuf::from("C:\\Code\\project"),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        session.project_id = Some("project-1".to_string());
        session.command_id = Some("command-1".to_string());
        session.tab_id = Some("tab-1".to_string());
        session.ai_launch = Some(AiLaunchSpec {
            tab_id: "tab-1".to_string(),
            project_id: "project-1".to_string(),
            tool: SessionKind::Claude,
            cwd: PathBuf::from("C:\\Code\\project"),
            shell_program: "pwsh".to_string(),
            shell_args: Vec::new(),
            startup_command: "STARTUP_SENTINEL".to_string(),
        });
        runtime.sessions.insert("session-1".to_string(), session);

        HostFixture {
            app,
            runtime,
            ports: HashMap::from([(
                43872,
                PortStatus {
                    port: 43872,
                    in_use: true,
                    pid: Some(42),
                    process_name: Some("server".to_string()),
                },
            )]),
            lease: WebWriterLeaseState::default(),
        }
    }

    #[test]
    fn browser_snapshot_never_serializes_host_secrets() {
        let fixture = host_fixture_with_sentinels();
        let value = serde_json::to_string(&WebWorkspaceSnapshot::from_host(
            "runtime-1",
            7,
            &fixture.app,
            &fixture.runtime,
            &fixture.ports,
            &fixture.lease,
        ))
        .unwrap();
        for forbidden in [
            "PASSWORD_SENTINEL",
            "PRIVATE_KEY_SENTINEL",
            "TOKEN_SENTINEL",
            "ENV_SENTINEL",
            "STARTUP_SENTINEL",
            "NOTES_SENTINEL",
        ] {
            assert!(!value.contains(forbidden), "leaked {forbidden}");
        }
    }

    #[test]
    fn browser_snapshot_contains_only_renderer_metadata_and_live_state() {
        let fixture = host_fixture_with_sentinels();
        let snapshot = WebWorkspaceSnapshot::from_host(
            "runtime-1",
            7,
            &fixture.app,
            &fixture.runtime,
            &fixture.ports,
            &fixture.lease,
        );

        assert_eq!(snapshot.web_protocol_version, WEB_PROTOCOL_VERSION);
        assert_eq!(snapshot.runtime_instance_id, "runtime-1");
        assert_eq!(snapshot.revision, 7);
        assert_eq!(snapshot.projects[0].id, "project-1");
        assert_eq!(snapshot.projects[0].folders[0].commands[0].id, "command-1");
        assert_eq!(
            snapshot.projects[0].folders[0].commands[0].status,
            crate::state::SessionStatus::Starting
        );
        assert_eq!(snapshot.ssh_connections[0].username, "dev");
        assert_eq!(snapshot.tabs[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(snapshot.sessions[0].session_id, "session-1");
        assert_eq!(snapshot.port_statuses[0].port, 43872);
    }
}
