use crate::models::{PortStatus, TabType};
use crate::remote::presentation::{
    SemanticAdapterHealth, SemanticAttention, SemanticSessionMetadata, StableSessionKey,
};
use crate::remote::{RemotePortAuthority, RemotePortAuthorityKind};
use crate::state::{
    AiActivity, AppState, RuntimeState, SessionDimensions, SessionKind, SessionRuntimeState,
    SessionStatus,
};
use serde::Serialize;
use std::collections::HashMap;

pub const WEB_PROTOCOL_VERSION: u32 = 3;
pub const WEB_BUILD_ID: &str = env!("DEVMANAGER_WEB_BUILD_ID");

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
    /// Typed authority is the source of colour/control decisions. The legacy
    /// status list remains only for older clients.
    pub port_authorities: Vec<WebPortAuthority>,
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
    pub stable_session_key: Option<StableSessionKey>,
    pub kind: WebSessionKind,
    pub status: SessionStatus,
    pub project_id: Option<String>,
    pub command_id: Option<String>,
    pub tab_id: Option<String>,
    pub dimensions: SessionDimensions,
    pub interactive_shell: bool,
    /// Host terminal/provider title already tracked for the native sidebar.
    pub title: Option<String>,
    /// Host AI activity already tracked for the native sidebar.
    pub ai_activity: Option<AiActivity>,
    /// Sticky semantic task title from the first substantive user message.
    pub task_title: Option<String>,
    pub last_activity_epoch_ms: Option<u64>,
    pub attention: SemanticAttention,
    pub attention_count: u64,
    pub adapter_health: SemanticAdapterHealth,
    pub raw_required: bool,
    pub oldest_sequence: u64,
    pub latest_sequence: u64,
}

impl WebSessionSummary {
    fn from_host(
        session: &SessionRuntimeState,
        app: &AppState,
        semantic_metadata: &HashMap<StableSessionKey, SemanticSessionMetadata>,
    ) -> Self {
        let stable_session_key = StableSessionKey::resolve(session, &app.open_tabs);
        let metadata = stable_session_key
            .as_ref()
            .and_then(|key| semantic_metadata.get(key).cloned())
            .unwrap_or_default();
        Self {
            session_id: session.session_id.clone(),
            stable_session_key,
            kind: session.session_kind.into(),
            status: session.status,
            project_id: session.project_id.clone(),
            command_id: session.command_id.clone(),
            tab_id: session.tab_id.clone(),
            dimensions: session.dimensions,
            interactive_shell: session.interactive_shell,
            title: session.title.clone(),
            ai_activity: session.ai_activity,
            task_title: metadata.task_title.clone(),
            last_activity_epoch_ms: metadata.last_activity_epoch_ms,
            attention: metadata.attention,
            attention_count: metadata.attention_count,
            adapter_health: metadata.adapter_health,
            raw_required: metadata.raw_required,
            oldest_sequence: metadata.oldest_sequence,
            latest_sequence: metadata.latest_sequence,
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

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WebPortAuthorityKind {
    Managed,
    ManagedUnready,
    ProvenExternal,
    Unknown,
    ProbeError,
    Free,
    Occupied,
}

impl From<RemotePortAuthorityKind> for WebPortAuthorityKind {
    fn from(value: RemotePortAuthorityKind) -> Self {
        match value {
            RemotePortAuthorityKind::Managed => Self::Managed,
            RemotePortAuthorityKind::ManagedUnready => Self::ManagedUnready,
            RemotePortAuthorityKind::ProvenExternal => Self::ProvenExternal,
            RemotePortAuthorityKind::Unknown => Self::Unknown,
            RemotePortAuthorityKind::ProbeError => Self::ProbeError,
            RemotePortAuthorityKind::Free => Self::Free,
            RemotePortAuthorityKind::Occupied => Self::Occupied,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WebPortControlReason {
    ExactManagedFence,
    ManagedUnready,
    ProvenExternalNoControl,
    Starting,
    Free,
    Stale,
    ProbeFault,
    MixedOrUnverified,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebPortAuthority {
    pub port: u16,
    pub kind: WebPortAuthorityKind,
    pub resource_generation: Option<u64>,
    pub listeners: Vec<WebPortListenerIdentity>,
    pub session_id: Option<String>,
    pub root: Option<WebPortListenerIdentity>,
    pub membership_revision: u64,
    pub observation_sequence: u64,
    pub publication_sequence: u64,
    pub observed_at_epoch_ms: u64,
    pub freshness_deadline_epoch_ms: u64,
    pub fresh: bool,
    /// The matching host session must not still be waiting for exact process
    /// cleanup before a browser action can use this authority.
    pub reap_incomplete: bool,
    pub control_reason: WebPortControlReason,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebPortListenerIdentity {
    pub pid: u32,
    pub creation_time_100ns: u64,
    pub executable_proven: bool,
}

impl WebPortAuthority {
    fn from_remote(
        authority: &RemotePortAuthority,
        runtime: &RuntimeState,
        now_epoch_ms: u64,
    ) -> Self {
        let kind = WebPortAuthorityKind::from(authority.kind);
        let fresh = authority.is_fresh_at(now_epoch_ms);
        let matching_session = authority
            .session_id
            .as_deref()
            .and_then(|session_id| {
                runtime
                    .sessions
                    .values()
                    .find(|session| session.session_id == session_id)
            })
            .or_else(|| {
                runtime.sessions.values().find(|session| {
                    session
                        .server_launch
                        .as_ref()
                        .and_then(|launch| launch.port)
                        == Some(authority.port)
                })
            });
        let host_verified_for_current_session = authority.is_host_verified()
            && matching_session.is_some_and(|session| {
                session.status == SessionStatus::Running
                    && !session.reap_incomplete
                    && authority.session_id.as_deref() == Some(session.session_id.as_str())
                    && session
                        .server_launch
                        .as_ref()
                        .and_then(|launch| launch.port)
                        == Some(authority.port)
            });
        let control_reason = if !fresh {
            WebPortControlReason::Stale
        } else {
            match authority.kind {
                RemotePortAuthorityKind::Managed if host_verified_for_current_session => {
                    WebPortControlReason::ExactManagedFence
                }
                RemotePortAuthorityKind::ManagedUnready if host_verified_for_current_session => {
                    WebPortControlReason::ManagedUnready
                }
                RemotePortAuthorityKind::ProvenExternal => {
                    WebPortControlReason::ProvenExternalNoControl
                }
                RemotePortAuthorityKind::Free => WebPortControlReason::Free,
                RemotePortAuthorityKind::ProbeError => WebPortControlReason::ProbeFault,
                RemotePortAuthorityKind::Unknown => WebPortControlReason::MixedOrUnverified,
                RemotePortAuthorityKind::Occupied => WebPortControlReason::MixedOrUnverified,
                RemotePortAuthorityKind::Managed | RemotePortAuthorityKind::ManagedUnready => {
                    WebPortControlReason::MixedOrUnverified
                }
            }
        };
        Self {
            port: authority.port,
            kind,
            resource_generation: authority
                .resource
                .map(|resource| resource.runtime_generation),
            listeners: authority
                .listeners
                .iter()
                .map(|listener| WebPortListenerIdentity {
                    pid: listener.pid,
                    creation_time_100ns: listener.creation_time_100ns,
                    executable_proven: listener.executable_proven,
                })
                .collect(),
            session_id: authority.session_id.clone(),
            root: authority.root.as_ref().map(|root| WebPortListenerIdentity {
                pid: root.pid,
                creation_time_100ns: root.creation_time_100ns,
                executable_proven: root.executable_proven,
            }),
            membership_revision: authority.membership_revision,
            observation_sequence: authority.observation_sequence,
            publication_sequence: authority.publication_sequence,
            observed_at_epoch_ms: authority.observed_at_epoch_ms,
            freshness_deadline_epoch_ms: authority.freshness_deadline_epoch_ms,
            fresh,
            reap_incomplete: matching_session
                .map(|session| session.reap_incomplete)
                .unwrap_or(matches!(
                    authority.kind,
                    RemotePortAuthorityKind::Managed | RemotePortAuthorityKind::ManagedUnready
                )),
            control_reason,
            error: authority.error.clone(),
        }
    }
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
        semantic_metadata: &HashMap<StableSessionKey, SemanticSessionMetadata>,
    ) -> Self {
        Self::from_host_with_authorities(
            runtime_instance_id,
            revision,
            app,
            runtime,
            ports,
            &HashMap::new(),
            lease,
            semantic_metadata,
        )
    }

    pub fn from_host_with_authorities(
        runtime_instance_id: impl Into<String>,
        revision: u64,
        app: &AppState,
        runtime: &RuntimeState,
        ports: &HashMap<u16, PortStatus>,
        authorities: &HashMap<u16, RemotePortAuthority>,
        lease: &WebWriterLeaseState,
        semantic_metadata: &HashMap<StableSessionKey, SemanticSessionMetadata>,
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
            .map(|session| WebSessionSummary::from_host(session, app, semantic_metadata))
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));

        let mut port_statuses = ports.values().map(WebPortStatus::from).collect::<Vec<_>>();
        port_statuses.sort_by_key(|status| status.port);
        let now_epoch_ms = crate::remote::now_epoch_ms();
        let mut port_authorities = authorities
            .values()
            .map(|authority| WebPortAuthority::from_remote(authority, runtime, now_epoch_ms))
            .collect::<Vec<_>>();
        port_authorities.sort_by_key(|authority| authority.port);

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
            port_authorities,
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
    use crate::remote::presentation::{
        SemanticAdapterHealth, SemanticAttention, SemanticEventDraft, SemanticEventKind,
        SemanticJournalStore, SemanticRetention, SemanticSource, StableSessionKey,
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
        journals: SemanticJournalStore,
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
            provider_session_id: None,
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
        runtime
            .sessions
            .insert("session-1".to_string(), session.clone());
        let mut journals = SemanticJournalStore::default();
        journals.observe_runtime(&session, &app.open_tabs, 6);

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
            journals,
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
            &fixture.journals.metadata_snapshot(),
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
        assert_eq!(WEB_PROTOCOL_VERSION, 3);
        let fixture = host_fixture_with_sentinels();
        let snapshot = WebWorkspaceSnapshot::from_host(
            "runtime-1",
            7,
            &fixture.app,
            &fixture.runtime,
            &fixture.ports,
            &fixture.lease,
            &fixture.journals.metadata_snapshot(),
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
        assert_eq!(
            snapshot.sessions[0].stable_session_key.as_ref(),
            Some(&StableSessionKey::from_tab("tab-1"))
        );
        assert_eq!(snapshot.sessions[0].last_activity_epoch_ms, Some(6));
        assert_eq!(snapshot.sessions[0].attention, SemanticAttention::None);
        assert_eq!(snapshot.sessions[0].attention_count, 0);
        assert_eq!(
            snapshot.sessions[0].adapter_health,
            SemanticAdapterHealth::Degraded
        );
        assert_eq!(snapshot.sessions[0].oldest_sequence, 1);
        assert_eq!(snapshot.sessions[0].latest_sequence, 1);
        assert_eq!(snapshot.port_statuses[0].port, 43872);
    }

    #[test]
    fn browser_managed_authority_requires_host_verified_projection() {
        let fixture = host_fixture_with_sentinels();
        let now = crate::remote::now_epoch_ms();
        let authority = RemotePortAuthority {
            port: 43872,
            kind: RemotePortAuthorityKind::Managed,
            resource: Some(crate::domain::operation::ResourceFence::new(
                crate::domain::id::ResourceId::new(),
                7,
            )),
            listeners: vec![crate::remote::RemoteListenerIdentity {
                pid: 4242,
                creation_time_100ns: 42_420_000,
                executable_proven: true,
                executable_fingerprint: None,
            }],
            session_id: Some("session-1".to_string()),
            root: Some(crate::remote::RemoteListenerIdentity {
                pid: 4242,
                creation_time_100ns: 42_420_000,
                executable_proven: true,
                executable_fingerprint: None,
            }),
            membership_revision: 9,
            observation_sequence: 11,
            publication_sequence: 13,
            observed_at_epoch_ms: now,
            freshness_deadline_epoch_ms: now + crate::remote::REMOTE_PORT_AUTHORITY_MAX_AGE_MS,
            managed_fence_fingerprint: Some(42),
            verified: None,
            error: None,
        };
        let snapshot = WebWorkspaceSnapshot::from_host_with_authorities(
            "runtime-1",
            7,
            &fixture.app,
            &fixture.runtime,
            &fixture.ports,
            &HashMap::from([(43872, authority)]),
            &fixture.lease,
            &fixture.journals.metadata_snapshot(),
        );

        assert!(
            matches!(
                snapshot.port_authorities[0].control_reason,
                WebPortControlReason::MixedOrUnverified
            ),
            "wire-shaped managed metadata cannot mint a Web control projection"
        );
    }

    #[test]
    fn browser_session_summary_projects_interactive_shell_state() {
        let mut fixture = host_fixture_with_sentinels();
        fixture
            .runtime
            .sessions
            .get_mut("session-1")
            .unwrap()
            .interactive_shell = true;
        let value = serde_json::to_value(WebWorkspaceSnapshot::from_host(
            "runtime-1",
            7,
            &fixture.app,
            &fixture.runtime,
            &fixture.ports,
            &fixture.lease,
            &fixture.journals.metadata_snapshot(),
        ))
        .unwrap();

        assert_eq!(value["sessions"][0]["interactiveShell"], true);
    }

    #[test]
    fn browser_session_summary_projects_runtime_title_and_ai_activity() {
        use crate::state::AiActivity;

        let mut fixture = host_fixture_with_sentinels();
        {
            let session = fixture.runtime.sessions.get_mut("session-1").unwrap();
            session.title = Some("Fix mobile sessions ordering".to_string());
            session.ai_activity = Some(AiActivity::Thinking);
        }
        let value = serde_json::to_value(WebWorkspaceSnapshot::from_host(
            "runtime-1",
            7,
            &fixture.app,
            &fixture.runtime,
            &fixture.ports,
            &fixture.lease,
            &fixture.journals.metadata_snapshot(),
        ))
        .unwrap();

        assert_eq!(
            value["sessions"][0]["title"],
            "Fix mobile sessions ordering"
        );
        assert_eq!(value["sessions"][0]["aiActivity"], "Thinking");
        assert_eq!(WEB_PROTOCOL_VERSION, 3);
    }

    #[test]
    fn browser_session_summary_projects_semantic_task_title() {
        let mut fixture = host_fixture_with_sentinels();
        let key = StableSessionKey::from_tab("tab-1");
        fixture.journals.record(SemanticEventDraft {
            stable_session_key: key,
            occurred_at_epoch_ms: 10,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::UserMessage {
                text: "  Investigate   househunter listing sync  ".to_string(),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: None,
        });
        let value = serde_json::to_value(WebWorkspaceSnapshot::from_host(
            "runtime-1",
            7,
            &fixture.app,
            &fixture.runtime,
            &fixture.ports,
            &fixture.lease,
            &fixture.journals.metadata_snapshot(),
        ))
        .unwrap();

        assert_eq!(
            value["sessions"][0]["taskTitle"],
            "Investigate househunter listing sync"
        );
        assert_eq!(WEB_PROTOCOL_VERSION, 3);
    }
}
