//! JSON-over-WebSocket wire protocol between the browser SPA and the in-process
//! `WebClientSession` bridge. Kept deliberately small and passthrough-friendly:
//! we reuse `RemoteAction`, `RemoteWorkspaceSnapshot`, `RemoteWorkspaceDelta`,
//! and `RemoteActionResult` directly because they already derive `Serialize` /
//! `Deserialize` and there is no benefit to inventing duplicate web-only
//! shapes.

use serde::{Deserialize, Serialize};

use super::super::{
    RemoteAction, RemoteActionResult, RemoteWorkspaceDelta, RemoteWorkspaceSnapshot,
};
use crate::terminal::session::TerminalScreenSnapshot;

/// Messages the browser sends to the host over the `/api/ws` text channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WsInbound {
    SubscribeSessions {
        session_ids: Vec<String>,
    },
    UnsubscribeSessions {
        session_ids: Vec<String>,
    },
    FocusSession {
        session_id: String,
    },
    Input {
        session_id: String,
        text: String,
    },
    PasteImage {
        session_id: String,
        mime_type: String,
        file_name: Option<String>,
        data_base64: String,
    },
    Resize {
        session_id: String,
        rows: u16,
        cols: u16,
    },
    Action {
        action: RemoteAction,
    },
    Request {
        id: u64,
        action: RemoteAction,
    },
    TakeControl,
    ReleaseControl,
    Ping,
}

/// Messages the host sends to the browser. Text frames carry JSON;
/// `session_output` payloads are sent as **binary** frames instead (see
/// `bridge::encode_session_output_frame`) to avoid base64 overhead in the hot
/// terminal stream path. Everything else is plain JSON.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WsOutbound {
    Hello {
        client_id: String,
        server_id: String,
        protocol_version: u32,
    },
    Snapshot {
        workspace: RemoteWorkspaceSnapshot,
    },
    Delta {
        delta: RemoteWorkspaceDelta,
    },
    ControlState {
        controller_client_id: Option<String>,
        you_have_control: bool,
    },
    SessionBootstrap {
        session_id: String,
        /// Raw PTY replay bytes, base64-encoded. Small enough at session-open
        /// time that we don't need a binary frame here.
        replay_base64: String,
        /// Exact host-side terminal snapshot for the current viewport.
        screen: TerminalScreenSnapshot,
    },
    SessionClosed {
        session_id: String,
    },
    SessionRemoved {
        session_id: String,
    },
    Response {
        id: u64,
        result: RemoteActionResult,
    },
    Error {
        message: String,
    },
    Pong,
    /// Emitted when the host is tearing the connection down. The client
    /// should not attempt to reconnect immediately.
    Disconnected {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inbound_subscribe_deserializes() {
        let raw = json!({
            "type": "subscribeSessions",
            "sessionIds": ["a", "b"]
        });
        let parsed: WsInbound = serde_json::from_value(raw).expect("parse");
        match parsed {
            WsInbound::SubscribeSessions { session_ids } => {
                assert_eq!(session_ids, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn inbound_input_deserializes() {
        let raw = json!({
            "type": "input",
            "sessionId": "srv-1",
            "text": "echo hi\n"
        });
        let parsed: WsInbound = serde_json::from_value(raw).expect("parse");
        match parsed {
            WsInbound::Input { session_id, text } => {
                assert_eq!(session_id, "srv-1");
                assert_eq!(text, "echo hi\n");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn inbound_paste_image_deserializes() {
        let raw = json!({
            "type": "pasteImage",
            "sessionId": "claude-1",
            "mimeType": "image/png",
            "fileName": "clip.png",
            "dataBase64": "AQID"
        });
        let parsed: WsInbound = serde_json::from_value(raw).expect("parse");
        match parsed {
            WsInbound::PasteImage {
                session_id,
                mime_type,
                file_name,
                data_base64,
            } => {
                assert_eq!(session_id, "claude-1");
                assert_eq!(mime_type, "image/png");
                assert_eq!(file_name.as_deref(), Some("clip.png"));
                assert_eq!(data_base64, "AQID");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn inbound_ping_has_no_payload() {
        let raw = json!({ "type": "ping" });
        let parsed: WsInbound = serde_json::from_value(raw).expect("parse");
        assert!(matches!(parsed, WsInbound::Ping));
    }

    #[test]
    fn inbound_take_control_has_no_payload() {
        let raw = json!({ "type": "takeControl" });
        let parsed: WsInbound = serde_json::from_value(raw).expect("parse");
        assert!(matches!(parsed, WsInbound::TakeControl));
    }

    #[test]
    fn outbound_pong_serializes_without_payload() {
        let value = serde_json::to_value(WsOutbound::Pong).unwrap();
        assert_eq!(value, json!({ "type": "pong" }));
    }

    #[test]
    fn outbound_error_has_message_field() {
        let value = serde_json::to_value(WsOutbound::Error {
            message: "nope".to_string(),
        })
        .unwrap();
        assert_eq!(value, json!({ "type": "error", "message": "nope" }));
    }

    #[test]
    fn outbound_control_state_serializes() {
        let value = serde_json::to_value(WsOutbound::ControlState {
            controller_client_id: Some("web-123".to_string()),
            you_have_control: true,
        })
        .unwrap();
        assert_eq!(
            value,
            json!({
                "type": "controlState",
                "controllerClientId": "web-123",
                "youHaveControl": true
            })
        );
    }
}
