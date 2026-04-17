use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RemoteAccessSource {
    #[default]
    Browser,
    NativeApp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RemoteAccessActivityKind {
    Paired,
    Connected,
    Reconnected,
}

impl Default for RemoteAccessActivityKind {
    fn default() -> Self {
        Self::Connected
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteAccessActivityEvent {
    pub client_id: String,
    pub source: RemoteAccessSource,
    pub event_kind: RemoteAccessActivityKind,
    pub label: String,
    pub ip_address: Option<String>,
    pub event_at_epoch_ms: Option<u64>,
    pub browser_family: Option<String>,
    pub browser_version: Option<String>,
    pub os_family: Option<String>,
    pub device_class: Option<String>,
}

impl Default for RemoteAccessActivityEvent {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            source: RemoteAccessSource::default(),
            event_kind: RemoteAccessActivityKind::default(),
            label: String::new(),
            ip_address: None,
            event_at_epoch_ms: None,
            browser_family: None,
            browser_version: None,
            os_family: None,
            device_class: None,
        }
    }
}
