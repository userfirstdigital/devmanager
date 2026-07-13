use crate::models::TabType;
use crate::remote::{RemoteAction, RemoteActionPayload, RemoteActionResult};
use crate::state::SessionDimensions;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebAiKind {
    Claude,
    Codex,
}

/// Browser-visible action result. This deliberately does not expose the
/// native action payload enum, whose variants can contain host runtime state.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebActionResult {
    pub ok: bool,
    pub message: Option<String>,
    pub payload: Option<WebActionPayload>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WebActionPayload {
    AiTab {
        tab_id: String,
        project_id: String,
        tab_type: WebAiKind,
        session_id: String,
        label: Option<String>,
    },
}

impl WebActionResult {
    pub fn from_remote(result: &RemoteActionResult) -> Self {
        Self {
            ok: result.ok,
            message: result.message.clone(),
            payload: result
                .payload
                .as_ref()
                .and_then(WebActionPayload::from_remote),
        }
    }
}

impl WebActionPayload {
    fn from_remote(payload: &RemoteActionPayload) -> Option<Self> {
        let RemoteActionPayload::AiTab {
            tab_id,
            project_id,
            tab_type,
            session_id,
            label,
            ..
        } = payload
        else {
            return None;
        };

        let tab_type = match tab_type {
            TabType::Claude => WebAiKind::Claude,
            TabType::Codex => WebAiKind::Codex,
            TabType::Server | TabType::Ssh => return None,
        };

        Some(Self::AiTab {
            tab_id: tab_id.clone(),
            project_id: project_id.clone(),
            tab_type,
            session_id: session_id.clone(),
            label: label.clone(),
        })
    }
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
