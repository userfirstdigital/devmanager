use crate::models::TabType;
use crate::remote::RemoteAction;
use crate::state::SessionDimensions;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebAiKind {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum WebAction {
    StartServer {
        command_id: String,
    },
    StopServer {
        command_id: String,
    },
    RestartServer {
        command_id: String,
    },
    LaunchAi {
        project_id: String,
        tab_type: WebAiKind,
    },
    RestartAiTab {
        tab_id: String,
    },
    CloseTab {
        tab_id: String,
    },
    OpenSshTab {
        connection_id: String,
    },
    ConnectSsh {
        connection_id: String,
    },
    RestartSsh {
        connection_id: String,
    },
    DisconnectSsh {
        connection_id: String,
    },
    StopAllServers,
}

impl WebAction {
    pub fn into_remote(self) -> RemoteAction {
        let dimensions = SessionDimensions::default();
        match self {
            Self::StartServer { command_id } => RemoteAction::StartServer {
                command_id,
                focus: true,
                dimensions,
            },
            Self::StopServer { command_id } => RemoteAction::StopServer { command_id },
            Self::RestartServer { command_id } => RemoteAction::RestartServer {
                command_id,
                dimensions,
            },
            Self::LaunchAi {
                project_id,
                tab_type,
            } => RemoteAction::LaunchAi {
                project_id,
                tab_type: match tab_type {
                    WebAiKind::Claude => TabType::Claude,
                    WebAiKind::Codex => TabType::Codex,
                },
                dimensions,
            },
            Self::RestartAiTab { tab_id } => RemoteAction::RestartAiTab { tab_id, dimensions },
            Self::CloseTab { tab_id } => RemoteAction::CloseTab { tab_id },
            Self::OpenSshTab { connection_id } => RemoteAction::OpenSshTab { connection_id },
            Self::ConnectSsh { connection_id } => RemoteAction::ConnectSsh {
                connection_id,
                dimensions,
            },
            Self::RestartSsh { connection_id } => RemoteAction::RestartSsh {
                connection_id,
                dimensions,
            },
            Self::DisconnectSsh { connection_id } => RemoteAction::DisconnectSsh { connection_id },
            Self::StopAllServers => RemoteAction::StopAllServers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::RemoteAction;
    use crate::state::SessionDimensions;

    #[test]
    fn web_action_parser_rejects_native_configuration_replacement() {
        let raw = serde_json::json!({"type":"saveSsh","connection":{"password":"secret"}});
        assert!(serde_json::from_value::<WebAction>(raw).is_err());
    }

    #[test]
    fn web_action_translation_supplies_host_owned_defaults() {
        let action = WebAction::StartServer {
            command_id: "command-1".to_string(),
        }
        .into_remote();

        match action {
            RemoteAction::StartServer {
                command_id,
                focus,
                dimensions,
            } => {
                assert_eq!(command_id, "command-1");
                assert!(focus);
                assert_eq!(dimensions, SessionDimensions::default());
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }
}
