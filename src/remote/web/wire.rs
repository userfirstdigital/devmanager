//! JSON-over-WebSocket wire protocol between the browser SPA and the in-process
//! `WebClientSession` bridge. Browser-visible workspace state and mutating
//! action inputs use explicit web-only allowlists instead of native workspace
//! snapshots, deltas, or `RemoteAction` values.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::action::{WebAction, WebActionResult};
use super::dto::{WebWorkspaceDelta, WebWorkspaceSnapshot, WebWriterLeaseState};
use crate::remote::presentation::{SemanticEvent, StableSessionKey};
use crate::terminal::session::TerminalScreenSnapshot;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResumeRequest {
    pub seen_runtime_instance_id: Option<String>,
    pub seen_revision: Option<u64>,
    pub route: String,
    pub desired_session_key: Option<StableSessionKey>,
    pub semantic_after_sequence: Option<u64>,
    pub client_instance_id: String,
    pub visible: bool,
    pub wants_writer_lease: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeState {
    pub runtime_instance_id: String,
    pub revision: u64,
    pub hard_reset: bool,
    pub route: String,
    pub desired_session_key: Option<StableSessionKey>,
    pub workspace: Option<WebWorkspaceSnapshot>,
    pub semantic_replay: Option<SemanticReplayDescriptor>,
    pub writer_lease: WebWriterLeaseState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticReplayDescriptor {
    pub replay_id: u64,
    pub stable_session_key: StableSessionKey,
    pub from_sequence: u64,
    pub through_sequence: u64,
    pub rollover: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticReplayPage {
    pub replay_id: u64,
    pub stable_session_key: StableSessionKey,
    pub from_sequence: u64,
    pub through_sequence: u64,
    pub next_sequence: u64,
    pub rollover: bool,
    pub complete: bool,
    pub events: Vec<Arc<SemanticEvent>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComposerAttachment {
    pub mime_type: String,
    pub file_name: Option<String>,
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ComposerAccepted {
    pub mutation_id: String,
    pub stable_session_key: StableSessionKey,
    pub accepted_sequence: u64,
    pub lease_generation: u64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ComposerRejectCode {
    InvalidRequest,
    SessionNotFound,
    AmbiguousSession,
    NativeControllerActive,
    LeaseBusy,
    StaleGeneration,
    MutationInFlight,
    MutationConflict,
    CapacityExceeded,
    PtyRejected,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerRejected {
    pub mutation_id: String,
    pub code: ComposerRejectCode,
    pub message: String,
    pub writer_lease: WebWriterLeaseState,
}

/// Messages the browser sends to the host over the `/api/ws` text channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WsInbound {
    Resume {
        #[serde(flatten)]
        request: ResumeRequest,
    },
    AcquireWriterLease {
        client_instance_id: String,
        visible: bool,
    },
    WriterLeaseHeartbeat {
        client_instance_id: String,
        expected_lease_generation: u64,
        visible: bool,
    },
    SetVisibility {
        client_instance_id: String,
        visible: bool,
    },
    ComposerSubmit {
        mutation_id: String,
        stable_session_key: StableSessionKey,
        text: String,
        attachments: Vec<ComposerAttachment>,
        expected_lease_generation: u64,
    },
    SubscribeSemantic {
        stable_session_key: StableSessionKey,
        after_sequence: u64,
    },
    UnsubscribeSemantic {
        stable_session_key: StableSessionKey,
    },
    InterruptSession {
        stable_session_key: StableSessionKey,
        expected_lease_generation: u64,
    },
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
        #[serde(default)]
        expected_lease_generation: Option<u64>,
    },
    PasteImage {
        session_id: String,
        mime_type: String,
        file_name: Option<String>,
        data_base64: String,
        #[serde(default)]
        expected_lease_generation: Option<u64>,
    },
    Resize {
        session_id: String,
        rows: u16,
        cols: u16,
        #[serde(default)]
        expected_lease_generation: Option<u64>,
    },
    Action {
        action: WebAction,
        #[serde(default)]
        expected_lease_generation: Option<u64>,
    },
    Request {
        id: u64,
        action: WebAction,
        #[serde(default)]
        expected_lease_generation: Option<u64>,
    },
    TakeControl,
    ClaimControlIfAvailable,
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
        web_build_id: String,
    },
    Snapshot {
        workspace: WebWorkspaceSnapshot,
    },
    Delta {
        delta: WebWorkspaceDelta,
    },
    ResumeState {
        #[serde(flatten)]
        state: ResumeState,
    },
    WriterLeaseState {
        writer_lease: WebWriterLeaseState,
    },
    SemanticReplayPage {
        #[serde(flatten)]
        page: SemanticReplayPage,
    },
    SemanticEvent {
        event: SemanticEvent,
    },
    ComposerAccepted {
        #[serde(flatten)]
        accepted: ComposerAccepted,
    },
    ComposerRejected {
        #[serde(flatten)]
        rejected: ComposerRejected,
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
        result: WebActionResult,
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
    use crate::remote::presentation::StableSessionKey;
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
            WsInbound::Input {
                session_id,
                text,
                expected_lease_generation,
            } => {
                assert_eq!(session_id, "srv-1");
                assert_eq!(text, "echo hi\n");
                assert_eq!(expected_lease_generation, None);
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
                expected_lease_generation,
            } => {
                assert_eq!(session_id, "claude-1");
                assert_eq!(mime_type, "image/png");
                assert_eq!(file_name.as_deref(), Some("clip.png"));
                assert_eq!(data_base64, "AQID");
                assert_eq!(expected_lease_generation, None);
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
    fn inbound_claim_control_if_available_has_no_payload() {
        let raw = json!({ "type": "claimControlIfAvailable" });
        let parsed: WsInbound = serde_json::from_value(raw).expect("parse");
        assert!(matches!(parsed, WsInbound::ClaimControlIfAvailable));
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

    #[test]
    fn resume_deserializes_as_one_atomic_request() {
        let raw = json!({
            "type": "resume",
            "seenRuntimeInstanceId": "runtime-old",
            "seenRevision": 41,
            "route": "/session/tab/tab-a",
            "desiredSessionKey": "tab:tab-a",
            "semanticAfterSequence": 12,
            "clientInstanceId": "tab-instance-a",
            "visible": true,
            "wantsWriterLease": true
        });

        let parsed: WsInbound = serde_json::from_value(raw).expect("parse resume");
        let WsInbound::Resume { request } = parsed else {
            panic!("expected resume frame");
        };
        assert_eq!(
            request.seen_runtime_instance_id.as_deref(),
            Some("runtime-old")
        );
        assert_eq!(request.seen_revision, Some(41));
        assert_eq!(request.route, "/session/tab/tab-a");
        assert_eq!(
            request
                .desired_session_key
                .as_ref()
                .map(StableSessionKey::as_str),
            Some("tab:tab-a")
        );
        assert_eq!(request.semantic_after_sequence, Some(12));
        assert_eq!(request.client_instance_id, "tab-instance-a");
        assert!(request.visible);
        assert!(request.wants_writer_lease);
    }

    #[test]
    fn composer_submit_deserializes_with_mutation_and_generation() {
        let raw = json!({
            "type": "composerSubmit",
            "mutationId": "mutation-1",
            "stableSessionKey": "tab:tab-a",
            "text": "run tests",
            "attachments": [],
            "expectedLeaseGeneration": 7
        });

        let parsed: WsInbound = serde_json::from_value(raw).expect("parse composer submit");
        let WsInbound::ComposerSubmit {
            mutation_id,
            stable_session_key,
            text,
            attachments,
            expected_lease_generation,
        } = parsed
        else {
            panic!("expected composer submit");
        };
        assert_eq!(mutation_id, "mutation-1");
        assert_eq!(stable_session_key.as_str(), "tab:tab-a");
        assert_eq!(text, "run tests");
        assert!(attachments.is_empty());
        assert_eq!(expected_lease_generation, 7);
    }

    #[test]
    fn generation_bearing_raw_input_remains_backward_compatible() {
        let raw = json!({
            "type": "input",
            "sessionId": "srv-1",
            "text": "pwd\n",
            "expectedLeaseGeneration": 9
        });
        let parsed: WsInbound = serde_json::from_value(raw).expect("parse generation input");
        let WsInbound::Input {
            expected_lease_generation,
            ..
        } = parsed
        else {
            panic!("expected input");
        };
        assert_eq!(expected_lease_generation, Some(9));
    }

    #[test]
    fn semantic_subscription_and_interrupt_frames_deserialize() {
        let subscribe: WsInbound = serde_json::from_value(json!({
            "type": "subscribeSemantic",
            "stableSessionKey": "tab:tab-a",
            "afterSequence": 12
        }))
        .expect("parse semantic subscribe");
        assert!(matches!(
            subscribe,
            WsInbound::SubscribeSemantic {
                after_sequence: 12,
                ..
            }
        ));

        let interrupt: WsInbound = serde_json::from_value(json!({
            "type": "interruptSession",
            "stableSessionKey": "tab:tab-a",
            "expectedLeaseGeneration": 9
        }))
        .expect("parse interrupt");
        assert!(matches!(
            interrupt,
            WsInbound::InterruptSession {
                expected_lease_generation: 9,
                ..
            }
        ));
    }

    #[test]
    fn semantic_event_is_a_standalone_web_only_frame() {
        use crate::remote::presentation::{SemanticEventKind, SemanticSource};

        let event = SemanticEvent {
            stable_session_key: StableSessionKey::from_tab("tab-a"),
            sequence: 13,
            replaces_sequence: None,
            occurred_at_epoch_ms: 1_234,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::AssistantMessage {
                message_id: "message-1".to_string(),
                text: "done".to_string(),
                streaming: false,
            },
        };
        let value = serde_json::to_value(WsOutbound::SemanticEvent {
            event: event.clone(),
        })
        .expect("serialize semantic event");
        assert_eq!(value["type"], "semanticEvent");
        assert_eq!(value["event"]["sequence"], 13);
        assert_eq!(value["event"]["stableSessionKey"], "tab:tab-a");
    }
}
