use super::{
    BrowserRecipeAction, BrowserRecipeLocator, BrowserRecipeValue, BrowserRecordingAction,
    BrowserRecordingActor, BrowserRecordingCommit, BrowserRecordingError, BrowserRecordingInstance,
    BrowserRevision, BrowserRisk, BrowserWorkflowRecorder,
};
use serde::de::{Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fmt;
use std::sync::OnceLock;
use std::sync::{
    mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
    Arc, Mutex,
};

pub const MAX_BROWSER_PAGE_RECORDING_IPC_BYTES: usize = 8 * 1024;
pub const MAX_BROWSER_PAGE_RECORDING_IPC_DEPTH: usize = 8;
pub const MAX_BROWSER_PAGE_RECORDING_IPC_STRINGS: usize = 64;
pub const MAX_BROWSER_PAGE_RECORDING_STRING_BYTES: usize = 1_024;
pub const MAX_BROWSER_PAGE_RECORDING_LOCATOR_FALLBACKS: usize = 4;
pub const MAX_BROWSER_PAGE_RECORDING_SELECT_VALUES: usize = 16;
const MAX_BROWSER_PAGE_RECORDING_IPC_CONTAINERS: usize = 32;
const MAX_BROWSER_PAGE_RECORDING_IPC_MEMBERS: usize = 64;
const MAX_JAVASCRIPT_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserPageRecordingIpcError {
    Inactive,
    AlreadyActive,
    Unavailable,
    HostFailure,
    TransportInvalidated,
    InvalidAuthority,
    Malformed,
    Oversized,
    TooDeep,
    TooManyItems,
    Untrusted,
    Replay,
    InvalidEvent,
}

impl fmt::Display for BrowserPageRecordingIpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Inactive => "browser page recording IPC is inactive",
            Self::AlreadyActive => "browser page recording IPC is already active",
            Self::Unavailable => "browser page recording IPC is unavailable",
            Self::HostFailure => "browser page recording host operation failed",
            Self::TransportInvalidated => {
                "browser page recording transport invalidated the incomplete recording"
            }
            Self::InvalidAuthority => "browser page recording authority is invalid",
            Self::Malformed => "browser page recording IPC is malformed",
            Self::Oversized => "browser page recording IPC is oversized",
            Self::TooDeep => "browser page recording IPC is too deeply nested",
            Self::TooManyItems => "browser page recording IPC has too many items",
            Self::Untrusted => "browser page recording IPC is untrusted",
            Self::Replay => "browser page recording IPC was already observed",
            Self::InvalidEvent => "browser page recording event is invalid",
        })
    }
}

impl std::error::Error for BrowserPageRecordingIpcError {}

#[derive(Clone)]
pub struct BrowserPageRecordingAuthority {
    instance: BrowserRecordingInstance,
    tab_id: String,
    revision: BrowserRevision,
    origin: String,
    nonce: String,
}

impl BrowserPageRecordingAuthority {
    pub fn new(
        instance: BrowserRecordingInstance,
        tab_id: impl Into<String>,
        revision: BrowserRevision,
        origin: impl Into<String>,
        nonce: impl Into<String>,
    ) -> Result<Self, BrowserPageRecordingIpcError> {
        let tab_id = tab_id.into();
        let origin = canonical_browser_page_origin(&origin.into())?;
        let nonce = nonce.into();
        if !valid_identifier(&tab_id, 256)
            || !(32..=64).contains(&nonce.len())
            || !nonce
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(BrowserPageRecordingIpcError::InvalidAuthority);
        }
        Ok(Self {
            instance,
            tab_id,
            revision,
            origin,
            nonce,
        })
    }

    pub(crate) fn instance_id(&self) -> u64 {
        self.instance.id()
    }

    pub(crate) fn nonce(&self) -> &str {
        &self.nonce
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BrowserPageRecordingWorkspace {
    project_id: String,
    ai_tab_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum BrowserPageRecordingActor {
    User,
    Agent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum BrowserPageRecordingSource {
    Page,
    Chrome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserPageRecordingEvent {
    Click {
        locator: BrowserRecipeLocator,
    },
    TextEdit {
        locator: BrowserRecipeLocator,
        edit: BrowserPageRecordingTextEdit,
    },
    Select {
        locator: BrowserRecipeLocator,
        values: Vec<String>,
    },
    Navigation {
        url: String,
    },
    Upload {
        locator: BrowserRecipeLocator,
    },
    Download {
        locator: BrowserRecipeLocator,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserPageRecordingTextEdit {
    Text { text: String },
    Clear {},
    Password {},
    Clipboard {},
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserPageRecordingEnvelope {
    version: u32,
    channel: String,
    workspace: BrowserPageRecordingWorkspace,
    tab_id: String,
    revision: BrowserRevision,
    instance_id: u64,
    sequence: u64,
    actor: BrowserPageRecordingActor,
    source: BrowserPageRecordingSource,
    origin: String,
    event: BrowserPageRecordingEvent,
    nonce: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BrowserPageRecordingEnvelopeDocument {
    version: u32,
    channel: String,
    workspace: BrowserPageRecordingWorkspace,
    tab_id: String,
    revision: BrowserRevision,
    instance_id: u64,
    sequence: u64,
    actor: BrowserPageRecordingActor,
    source: BrowserPageRecordingSource,
    origin: String,
    event: BrowserPageRecordingEvent,
    nonce: String,
}

impl BrowserPageRecordingEnvelope {
    pub fn parse(body: &str) -> Result<Self, BrowserPageRecordingIpcError> {
        preflight_json(body)?;
        let strict: StrictJsonValue =
            serde_json::from_str(body).map_err(|_| BrowserPageRecordingIpcError::Malformed)?;
        let document: BrowserPageRecordingEnvelopeDocument = serde_json::from_value(strict.0)
            .map_err(|_| BrowserPageRecordingIpcError::Malformed)?;
        let origin = canonical_browser_page_origin(&document.origin)
            .map_err(|_| BrowserPageRecordingIpcError::Malformed)?;
        let envelope = Self {
            version: document.version,
            channel: document.channel,
            workspace: document.workspace,
            tab_id: document.tab_id,
            revision: document.revision,
            instance_id: document.instance_id,
            sequence: document.sequence,
            actor: document.actor,
            source: document.source,
            origin,
            event: document.event,
            nonce: document.nonce,
        };
        envelope.validate_shape()?;
        Ok(envelope)
    }

    pub fn event(&self) -> &BrowserPageRecordingEvent {
        &self.event
    }

    fn validate_shape(&self) -> Result<(), BrowserPageRecordingIpcError> {
        if self.version != 1
            || self.channel != "browserRecording"
            || !valid_identifier(&self.workspace.project_id, 256)
            || !valid_identifier(&self.workspace.ai_tab_id, 256)
            || !valid_identifier(&self.tab_id, 256)
            || self.instance_id == 0
            || self.sequence > MAX_JAVASCRIPT_SAFE_INTEGER
            || !(32..=64).contains(&self.nonce.len())
            || !self
                .nonce
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(BrowserPageRecordingIpcError::Malformed);
        }
        self.event.validate()?;
        Ok(())
    }
}

impl BrowserPageRecordingEvent {
    fn validate(&self) -> Result<(), BrowserPageRecordingIpcError> {
        match self {
            Self::TextEdit {
                edit: BrowserPageRecordingTextEdit::Text { text },
                ..
            } if text.len() > MAX_BROWSER_PAGE_RECORDING_STRING_BYTES
                || contains_sensitive_page_text(text)
                || text.chars().any(|character| {
                    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                }) =>
            {
                Err(BrowserPageRecordingIpcError::Malformed)
            }
            Self::Select { values, .. }
                if values.len() > MAX_BROWSER_PAGE_RECORDING_SELECT_VALUES
                    || values.iter().any(|value| {
                        value.len() > 512
                            || contains_sensitive_page_text(value)
                            || value.chars().any(|character| {
                                character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                            })
                    }) =>
            {
                Err(BrowserPageRecordingIpcError::Malformed)
            }
            Self::Navigation { url } if url.len() > 4_000 || contains_sensitive_page_text(url) => {
                Err(BrowserPageRecordingIpcError::Malformed)
            }
            _ => self
                .locators()
                .into_iter()
                .try_for_each(validate_locator_bounds),
        }
    }

    fn locators(&self) -> Vec<&BrowserRecipeLocator> {
        match self {
            Self::Click { locator }
            | Self::TextEdit { locator, .. }
            | Self::Select { locator, .. }
            | Self::Upload { locator }
            | Self::Download { locator } => vec![locator],
            Self::Navigation { .. } => Vec::new(),
        }
    }

    fn risk(&self) -> BrowserRisk {
        match self {
            Self::TextEdit {
                edit:
                    BrowserPageRecordingTextEdit::Password {}
                    | BrowserPageRecordingTextEdit::Clipboard {},
                ..
            } => BrowserRisk::AccountSecurity,
            _ => BrowserRisk::Normal,
        }
    }

    fn into_recording_action(self) -> Result<BrowserRecordingAction, BrowserRecordingError> {
        match self {
            Self::Click { locator } => {
                BrowserRecordingAction::recipe(BrowserRecipeAction::Click { locator })
            }
            Self::TextEdit { locator, edit } => match edit {
                BrowserPageRecordingTextEdit::Text { text } => {
                    BrowserRecordingAction::type_text(locator, &text)
                }
                BrowserPageRecordingTextEdit::Clear {} => {
                    BrowserRecordingAction::type_text(locator, "")
                }
                BrowserPageRecordingTextEdit::Password {} => {
                    BrowserRecordingAction::type_password(locator)
                }
                BrowserPageRecordingTextEdit::Clipboard {} => {
                    BrowserRecordingAction::type_clipboard(locator)
                }
            },
            Self::Select { locator, values } => {
                BrowserRecordingAction::recipe(BrowserRecipeAction::Select {
                    locator,
                    values: values
                        .into_iter()
                        .map(|value| BrowserRecipeValue::Literal { value })
                        .collect(),
                })
            }
            Self::Navigation { url } => BrowserRecordingAction::navigate(&url),
            Self::Upload { locator } => BrowserRecordingAction::upload(locator),
            Self::Download { locator } => {
                BrowserRecordingAction::recipe(BrowserRecipeAction::Download { locator })
            }
        }
    }
}

#[derive(Default)]
pub struct BrowserPageRecordingIpc {
    authority: Option<BrowserPageRecordingAuthority>,
    last_sequence: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserPageRecordingSubmit {
    Accepted,
    Fenced,
    Inactive,
    Stale,
    Invalid,
    Overflow,
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserPageRecordingTransportFailureKind {
    Overflow,
    Disconnected,
}

pub(crate) struct BrowserPageRecordingRawMessage {
    pub(crate) workspace_key: super::BrowserWorkspaceKey,
    pub(crate) tab_id: String,
    pub(crate) observed_origin: String,
    pub(crate) body: String,
    pub(crate) instance_id: u64,
}

pub(crate) struct BrowserPageRecordingTransportFailure {
    pub(crate) workspace_key: super::BrowserWorkspaceKey,
    pub(crate) tab_id: String,
    pub(crate) instance_id: u64,
    pub(crate) kind: BrowserPageRecordingTransportFailureKind,
}

pub(crate) struct BrowserPageRecordingTransportBatch {
    pub(crate) messages: Vec<BrowserPageRecordingRawMessage>,
    pub(crate) failures: Vec<BrowserPageRecordingTransportFailure>,
}

struct BrowserPageRecordingTransportIdentity {
    instance_id: u64,
    nonce: String,
}

#[derive(Clone)]
pub(crate) struct BrowserPageRecordingIngress {
    sender: SyncSender<BrowserPageRecordingRawMessage>,
    failure_sender: Sender<BrowserPageRecordingTransportFailure>,
    workspace_key: super::BrowserWorkspaceKey,
    tab_id: String,
    active: Arc<Mutex<Option<BrowserPageRecordingTransportIdentity>>>,
}

pub(crate) struct BrowserPageRecordingTransport {
    sender: SyncSender<BrowserPageRecordingRawMessage>,
    receiver: Receiver<BrowserPageRecordingRawMessage>,
    failure_sender: Sender<BrowserPageRecordingTransportFailure>,
    failure_receiver: Receiver<BrowserPageRecordingTransportFailure>,
}

#[cfg(test)]
fn browser_page_recording_transport(
    capacity: usize,
    workspace_key: super::BrowserWorkspaceKey,
    tab_id: String,
) -> (BrowserPageRecordingTransport, BrowserPageRecordingIngress) {
    let transport = BrowserPageRecordingTransport::with_capacity(capacity);
    let ingress = transport.ingress(workspace_key, tab_id);
    (transport, ingress)
}

impl BrowserPageRecordingTransport {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let (failure_sender, failure_receiver) = mpsc::channel();
        Self {
            sender,
            receiver,
            failure_sender,
            failure_receiver,
        }
    }

    pub(crate) fn ingress(
        &self,
        workspace_key: super::BrowserWorkspaceKey,
        tab_id: String,
    ) -> BrowserPageRecordingIngress {
        BrowserPageRecordingIngress {
            sender: self.sender.clone(),
            failure_sender: self.failure_sender.clone(),
            workspace_key,
            tab_id,
            active: Arc::new(Mutex::new(None)),
        }
    }
}

impl BrowserPageRecordingIngress {
    pub(crate) fn activate(
        &self,
        instance_id: u64,
        nonce: &str,
    ) -> Result<(), BrowserPageRecordingIpcError> {
        if instance_id == 0
            || !(32..=64).contains(&nonce.len())
            || !nonce
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(BrowserPageRecordingIpcError::InvalidAuthority);
        }
        let mut active = self
            .active
            .lock()
            .map_err(|_| BrowserPageRecordingIpcError::HostFailure)?;
        *active = Some(BrowserPageRecordingTransportIdentity {
            instance_id,
            nonce: nonce.to_string(),
        });
        Ok(())
    }

    pub(crate) fn fence(&self, instance_id: u64, nonce: &str) -> BrowserPageRecordingSubmit {
        let Ok(mut active) = self.active.lock() else {
            return BrowserPageRecordingSubmit::Disconnected;
        };
        match active.as_ref() {
            Some(identity) if identity.instance_id == instance_id && identity.nonce == nonce => {
                *active = None;
                BrowserPageRecordingSubmit::Fenced
            }
            Some(_) => BrowserPageRecordingSubmit::Stale,
            None => BrowserPageRecordingSubmit::Inactive,
        }
    }

    pub(crate) fn submit(&self, observed_origin: &str, body: String) -> BrowserPageRecordingSubmit {
        let Ok(envelope) = BrowserPageRecordingEnvelope::parse(&body) else {
            return BrowserPageRecordingSubmit::Invalid;
        };
        if envelope.workspace.project_id != self.workspace_key.project_id
            || envelope.workspace.ai_tab_id != self.workspace_key.ai_tab_id
            || envelope.tab_id != self.tab_id
        {
            return BrowserPageRecordingSubmit::Stale;
        }
        let Ok(mut active) = self.active.lock() else {
            return BrowserPageRecordingSubmit::Disconnected;
        };
        let Some(identity) = active.as_ref() else {
            return BrowserPageRecordingSubmit::Inactive;
        };
        if envelope.instance_id != identity.instance_id || envelope.nonce != identity.nonce {
            return BrowserPageRecordingSubmit::Stale;
        }
        let instance_id = identity.instance_id;
        let message = BrowserPageRecordingRawMessage {
            workspace_key: self.workspace_key.clone(),
            tab_id: self.tab_id.clone(),
            observed_origin: observed_origin.to_string(),
            body,
            instance_id,
        };
        match self.sender.try_send(message) {
            Ok(()) => BrowserPageRecordingSubmit::Accepted,
            Err(TrySendError::Full(_)) => {
                *active = None;
                let _ = self
                    .failure_sender
                    .send(BrowserPageRecordingTransportFailure {
                        workspace_key: self.workspace_key.clone(),
                        tab_id: self.tab_id.clone(),
                        instance_id,
                        kind: BrowserPageRecordingTransportFailureKind::Overflow,
                    });
                BrowserPageRecordingSubmit::Overflow
            }
            Err(TrySendError::Disconnected(_)) => {
                *active = None;
                let _ = self
                    .failure_sender
                    .send(BrowserPageRecordingTransportFailure {
                        workspace_key: self.workspace_key.clone(),
                        tab_id: self.tab_id.clone(),
                        instance_id,
                        kind: BrowserPageRecordingTransportFailureKind::Disconnected,
                    });
                BrowserPageRecordingSubmit::Disconnected
            }
        }
    }
}

impl BrowserPageRecordingTransport {
    pub(crate) fn drain(&self) -> BrowserPageRecordingTransportBatch {
        BrowserPageRecordingTransportBatch {
            messages: self.receiver.try_iter().collect(),
            failures: self.failure_receiver.try_iter().collect(),
        }
    }
}

impl BrowserPageRecordingIpc {
    pub fn activate(
        &mut self,
        authority: BrowserPageRecordingAuthority,
    ) -> Result<(), BrowserPageRecordingIpcError> {
        if self.authority.is_some() {
            return Err(BrowserPageRecordingIpcError::AlreadyActive);
        }
        self.authority = Some(authority);
        self.last_sequence = None;
        Ok(())
    }

    pub fn deactivate(&mut self) {
        self.authority = None;
        self.last_sequence = None;
    }

    pub(crate) fn fence_transport(
        &self,
        ingress: &BrowserPageRecordingIngress,
    ) -> BrowserPageRecordingSubmit {
        let Some(authority) = self.authority.as_ref() else {
            return BrowserPageRecordingSubmit::Inactive;
        };
        ingress.fence(authority.instance.id(), &authority.nonce)
    }

    pub fn activation_script(&self) -> Result<String, BrowserPageRecordingIpcError> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(BrowserPageRecordingIpcError::Inactive)?;
        let next_sequence = self
            .last_sequence
            .map(|sequence| sequence.saturating_add(1))
            .unwrap_or(0);
        if next_sequence > MAX_JAVASCRIPT_SAFE_INTEGER {
            return Err(BrowserPageRecordingIpcError::InvalidEvent);
        }
        let context = serde_json::json!({
            "version": 1,
            "channel": "browserRecording",
            "workspace": {
                "projectId": authority.instance.workspace_key().project_id,
                "aiTabId": authority.instance.workspace_key().ai_tab_id,
            },
            "tabId": authority.tab_id,
            "revision": authority.revision.0,
            "instanceId": authority.instance.id(),
            "actor": "user",
            "source": "page",
            "origin": authority.origin,
            "nonce": authority.nonce,
        });
        let context = serde_json::to_string(&context)
            .map_err(|_| BrowserPageRecordingIpcError::InvalidAuthority)?;
        Ok(PAGE_RECORDING_SCRIPT_TEMPLATE
            .replace("__DEVMANAGER_RECORDING_CONTEXT__", &context)
            .replace(
                "__DEVMANAGER_RECORDING_SEQUENCE__",
                &next_sequence.to_string(),
            ))
    }

    pub fn deactivation_script(&self) -> Result<String, BrowserPageRecordingIpcError> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(BrowserPageRecordingIpcError::Inactive)?;
        let nonce = serde_json::to_string(&authority.nonce)
            .map_err(|_| BrowserPageRecordingIpcError::InvalidAuthority)?;
        Ok(format!(
            r#"(() => {{ const active = window.__devmanagerBrowserRecording; if (active && typeof active.stop === "function") active.stop({nonce}, {}); }})();"#,
            authority.instance.id()
        ))
    }

    pub fn ingest(
        &mut self,
        recorder: &mut BrowserWorkflowRecorder,
        body: &str,
    ) -> Result<BrowserRecordingCommit, BrowserPageRecordingIpcError> {
        let observed_origin = self
            .authority
            .as_ref()
            .map(|authority| authority.origin.clone())
            .ok_or(BrowserPageRecordingIpcError::Inactive)?;
        self.ingest_from_origin(recorder, &observed_origin, body)
    }

    pub fn ingest_from_origin(
        &mut self,
        recorder: &mut BrowserWorkflowRecorder,
        observed_origin: &str,
        body: &str,
    ) -> Result<BrowserRecordingCommit, BrowserPageRecordingIpcError> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(BrowserPageRecordingIpcError::Inactive)?;
        let observed_origin = canonical_browser_page_origin(observed_origin)
            .map_err(|_| BrowserPageRecordingIpcError::Untrusted)?;
        if observed_origin != authority.origin {
            return Err(BrowserPageRecordingIpcError::Untrusted);
        }
        let envelope = BrowserPageRecordingEnvelope::parse(body)?;
        if !authority_matches(authority, &envelope) {
            return Err(BrowserPageRecordingIpcError::Untrusted);
        }
        if self
            .last_sequence
            .is_some_and(|sequence| envelope.sequence <= sequence)
        {
            return Err(BrowserPageRecordingIpcError::Replay);
        }

        let risk = envelope.event.risk();
        let reservation = recorder
            .reserve_on(
                &authority.instance,
                BrowserRecordingActor::User,
                &authority.tab_id,
                risk,
            )
            .map_err(map_recording_error)?;
        let action = envelope.event.into_recording_action();
        let action = match action {
            Ok(action) => action,
            Err(_) => {
                let _ = recorder.cancel(reservation);
                return Err(BrowserPageRecordingIpcError::InvalidEvent);
            }
        };
        let committed = recorder
            .commit(reservation, action)
            .map_err(map_recording_error)?;
        self.last_sequence = Some(envelope.sequence);
        Ok(committed)
    }
}

const PAGE_RECORDING_SCRIPT_TEMPLATE: &str = r#"
(() => {
  const marker = "__devmanagerBrowserRecording";
  const context = Object.freeze(__DEVMANAGER_RECORDING_CONTEXT__);
  const previous = window[marker];
  if (previous && typeof previous.stop === "function") previous.stop();

  let active = true;
  let sequence = __DEVMANAGER_RECORDING_SEQUENCE__;
  const listeners = [];
  const listen = (target, name, handler) => {
    target.addEventListener(name, handler, true);
    listeners.push([target, name, handler]);
  };
  const bounded = (value, maximum) => String(value ?? "")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, " ")
    .trim()
    .slice(0, maximum);
  const credentialText = (value) => {
    const text = String(value ?? "");
    return /\b(?:Basic|Bearer)\s+[A-Za-z0-9._~+\/=\-]+/i.test(text) ||
      /(?:authorization|password|passwd|token|secret|cookie|api[_-]?key|private[_-]?key)\s*[:=]\s*\S+/i.test(text) ||
      /["'](?:authorization|password|passwd|token|secret|cookie|api[_-]?key|private[_-]?key)["']\s*:\s*["'][^"']+/i.test(text) ||
      /(?:^|[^A-Za-z0-9_-])eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}(?:$|[^A-Za-z0-9_-])/.test(text) ||
      /(?:^|[^A-Za-z0-9_-])sk-(?:proj-)?[A-Za-z0-9_-]{20,}(?:$|[^A-Za-z0-9_-])/i.test(text) ||
      /(?:^|[^A-Za-z0-9_-])gh[pousr]_[A-Za-z0-9]{20,}(?:$|[^A-Za-z0-9])/i.test(text) ||
      /(?:^|[^A-Z0-9])(?:AKIA|ASIA)[A-Z0-9]{16}(?:$|[^A-Z0-9])/.test(text) ||
      /(?:^|[^A-Za-z0-9_-])AIza[A-Za-z0-9_-]{30,}(?:$|[^A-Za-z0-9_-])/.test(text);
  };
  const safeMetadata = (value, maximum) => {
    const text = bounded(value, maximum);
    return text && !credentialText(text) ? text : null;
  };
  const safeNavigationUrl = (value) => {
    try {
      const parsed = new URL(String(value), location.href);
      parsed.username = "";
      parsed.password = "";
      for (const key of [...parsed.searchParams.keys()]) {
        if (/authorization|password|passwd|token|secret|cookie|key/i.test(key)) {
          parsed.searchParams.delete(key);
        }
      }
      if (/authorization|password|passwd|token|secret|cookie|key/i.test(parsed.hash)) {
        parsed.hash = "";
      }
      const result = parsed.toString();
      return result.length <= 4000 && !credentialText(result) ? result : null;
    } catch (_) {
      return null;
    }
  };
  const implicitRole = (element) => {
    const tag = element.tagName?.toLowerCase();
    if (tag === "button") return "button";
    if (tag === "a" && element.hasAttribute?.("href")) return "link";
    if (tag === "textarea") return "textbox";
    if (tag === "select") return "combobox";
    if (tag === "input") {
      const type = String(element.getAttribute?.("type") || "text").toLowerCase();
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      if (["button", "submit", "reset"].includes(type)) return "button";
      return "textbox";
    }
    return null;
  };
  const semanticName = (element) => safeMetadata(
    element.getAttribute?.("aria-label") ||
    element.getAttribute?.("alt") ||
    element.getAttribute?.("title") ||
    "",
    256,
  );
  const escaped = (value) => window.CSS?.escape
    ? window.CSS.escape(String(value))
    : String(value).replace(/[^a-zA-Z0-9_-]/g, (character) => `\\${character}`);
  const locatorOf = (element) => {
    if (!(element instanceof Element)) return null;
    const role = safeMetadata(element.getAttribute?.("role") || implicitRole(element), 64);
    const name = semanticName(element);
    const testId = safeMetadata(element.getAttribute?.("data-testid"), 256);
    const cssSelectors = [];
    const id = safeMetadata(element.id, 256);
    if (id) cssSelectors.push(`#${escaped(id)}`);
    const fieldName = safeMetadata(element.getAttribute?.("name"), 256);
    if (fieldName && element.tagName) {
      cssSelectors.push(`${element.tagName.toLowerCase()}[name="${escaped(fieldName)}"]`);
    }
    const parent = element.parentElement;
    if (parent && element.tagName) {
      const siblings = [...parent.children].filter((child) => child.tagName === element.tagName);
      const position = siblings.indexOf(element);
      if (position >= 0) cssSelectors.push(`${element.tagName.toLowerCase()}:nth-of-type(${position + 1})`);
    }
    const locator = {
      accessibilityRole: role && name ? role : null,
      accessibilityName: role && name ? name : null,
      testId,
      cssSelectors: cssSelectors.slice(0, 4),
    };
    return locator.testId || locator.accessibilityRole || locator.cssSelectors.length ? locator : null;
  };
  const emit = (event) => {
    if (!active || location.origin !== context.origin || !event) return;
    if (!window.ipc || typeof window.ipc.postMessage !== "function") return;
    const message = { ...context, sequence, event };
    sequence += 1;
    window.ipc.postMessage(JSON.stringify(message));
  };
  const recordingTarget = (event) => {
    if (!event.isTrusted || !(event.target instanceof Element)) return null;
    if (event.target.closest?.("[data-devmanager-annotation-overlay]")) return null;
    return event.target;
  };
  const onInput = (event) => {
    const element = recordingTarget(event);
    if (!element) return;
    const locator = locatorOf(element);
    if (!locator) return;
    const tag = element.tagName?.toLowerCase();
    const type = String(element.getAttribute?.("type") || "").toLowerCase();
    if (type === "file") {
      emit({ type: "upload", locator });
      return;
    }
    if (tag === "select") {
      const values = [...(element.options || [])]
        .filter((option) => option.selected)
        .slice(0, 16)
        .map((option) => bounded(option.value, 512));
      if (values.some(credentialText)) {
        emit({ type: "textEdit", locator, edit: { type: "password" } });
        return;
      }
      emit({ type: "select", locator, values });
      return;
    }
    if (type === "password") {
      emit({ type: "textEdit", locator, edit: { type: "password" } });
      return;
    }
    if (event.inputType === "insertFromPaste") {
      emit({ type: "textEdit", locator, edit: { type: "clipboard" } });
      return;
    }
    const text = String(element.value ?? "").slice(0, 1024);
    if (credentialText(text)) {
      emit({ type: "textEdit", locator, edit: { type: "password" } });
      return;
    }
    emit({
      type: "textEdit",
      locator,
      edit: text ? { type: "text", text } : { type: "clear" },
    });
  };
  const onClick = (event) => {
    const element = recordingTarget(event);
    if (!element) return;
    const download = element.closest?.("a[download]");
    const locator = locatorOf(download || element);
    if (!locator) return;
    emit(download ? { type: "download", locator } : { type: "click", locator });
  };
  const onNavigation = (event) => {
    if (!event.isTrusted) return;
    const url = safeNavigationUrl(location.href);
    if (url) emit({ type: "navigation", url });
  };
  const stop = (nonce, instanceId) => {
    if (nonce !== undefined && (nonce !== context.nonce || instanceId !== context.instanceId)) return false;
    if (!active) return true;
    active = false;
    for (const [target, name, handler] of listeners.splice(0)) {
      target.removeEventListener(name, handler, true);
    }
    if (window[marker]?.stop === stop) delete window[marker];
    return true;
  };

  listen(document, "input", onInput);
  listen(document, "click", onClick);
  listen(window, "popstate", onNavigation);
  listen(window, "hashchange", onNavigation);
  Object.defineProperty(window, marker, {
    configurable: true,
    enumerable: false,
    value: Object.freeze({ stop }),
  });
})();
"#;

fn authority_matches(
    authority: &BrowserPageRecordingAuthority,
    envelope: &BrowserPageRecordingEnvelope,
) -> bool {
    envelope.workspace.project_id == authority.instance.workspace_key().project_id
        && envelope.workspace.ai_tab_id == authority.instance.workspace_key().ai_tab_id
        && envelope.tab_id == authority.tab_id
        && envelope.revision == authority.revision
        && envelope.instance_id == authority.instance.id()
        && envelope.actor == BrowserPageRecordingActor::User
        && envelope.source == BrowserPageRecordingSource::Page
        && envelope.origin == authority.origin
        && envelope.nonce.as_bytes() == authority.nonce.as_bytes()
}

fn map_recording_error(error: BrowserRecordingError) -> BrowserPageRecordingIpcError {
    match error {
        BrowserRecordingError::StaleInstance | BrowserRecordingError::StaleReservation => {
            BrowserPageRecordingIpcError::Untrusted
        }
        BrowserRecordingError::AlreadyActive
        | BrowserRecordingError::CapacityExceeded
        | BrowserRecordingError::InvalidAction
        | BrowserRecordingError::InvalidMutation => BrowserPageRecordingIpcError::InvalidEvent,
    }
}

fn valid_identifier(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= maximum
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn validate_locator_bounds(
    locator: &BrowserRecipeLocator,
) -> Result<(), BrowserPageRecordingIpcError> {
    if locator
        .accessibility_role
        .as_deref()
        .is_some_and(contains_sensitive_page_text)
        || locator
            .accessibility_name
            .as_deref()
            .is_some_and(contains_sensitive_page_text)
        || locator
            .test_id
            .as_deref()
            .is_some_and(contains_sensitive_page_text)
        || locator
            .css_selectors
            .iter()
            .any(|selector| contains_sensitive_page_text(selector))
    {
        return Err(BrowserPageRecordingIpcError::Malformed);
    }
    if locator.css_selectors.len() > MAX_BROWSER_PAGE_RECORDING_LOCATOR_FALLBACKS
        || locator
            .accessibility_role
            .as_ref()
            .is_some_and(|value| value.len() > 64)
        || locator
            .accessibility_name
            .as_ref()
            .is_some_and(|value| value.len() > 256)
        || locator
            .test_id
            .as_ref()
            .is_some_and(|value| value.len() > 256)
        || locator
            .css_selectors
            .iter()
            .any(|selector| selector.len() > 512)
    {
        return Err(BrowserPageRecordingIpcError::TooManyItems);
    }
    Ok(())
}

fn contains_sensitive_page_text(value: &str) -> bool {
    static SENSITIVE: OnceLock<regex::Regex> = OnceLock::new();
    SENSITIVE
        .get_or_init(|| {
            regex::Regex::new(
                r"(?ix)
                (?:^|[^A-Za-z0-9_-])eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}(?:$|[^A-Za-z0-9_-])
                |(?:^|[^A-Za-z0-9_-])sk-(?:proj-)?[A-Za-z0-9_-]{20,}(?:$|[^A-Za-z0-9_-])
                |(?:^|[^A-Za-z0-9_-])gh[pousr]_[A-Za-z0-9]{20,}(?:$|[^A-Za-z0-9])
                |(?:^|[^A-Z0-9])(?:AKIA|ASIA)[A-Z0-9]{16}(?:$|[^A-Z0-9])
                |(?:^|[^A-Za-z0-9_-])AIza[A-Za-z0-9_-]{30,}(?:$|[^A-Za-z0-9_-])",
            )
            .expect("static sensitive page text regex")
        })
        .is_match(value)
}

pub fn canonical_browser_page_origin(value: &str) -> Result<String, BrowserPageRecordingIpcError> {
    canonical_browser_page_origin_inner(value, true)
}

pub(crate) fn browser_page_origin_from_url(value: &str) -> Option<String> {
    canonical_browser_page_origin_inner(value, false).ok()
}

fn canonical_browser_page_origin_inner(
    value: &str,
    require_origin_only: bool,
) -> Result<String, BrowserPageRecordingIpcError> {
    if value.len() > 4_000 || value.chars().any(char::is_control) {
        return Err(BrowserPageRecordingIpcError::InvalidAuthority);
    }
    let parsed =
        url::Url::parse(value).map_err(|_| BrowserPageRecordingIpcError::InvalidAuthority)?;
    if !matches!(parsed.scheme(), "https" | "http")
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || (require_origin_only
            && (parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some()))
    {
        return Err(BrowserPageRecordingIpcError::InvalidAuthority);
    }
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" || origin.len() > 512 {
        return Err(BrowserPageRecordingIpcError::InvalidAuthority);
    }
    Ok(origin)
}

fn preflight_json(body: &str) -> Result<(), BrowserPageRecordingIpcError> {
    if body.len() > MAX_BROWSER_PAGE_RECORDING_IPC_BYTES {
        return Err(BrowserPageRecordingIpcError::Oversized);
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_bytes = 0usize;
    let mut string_count = 0usize;
    let mut container_count = 0usize;
    let mut member_count = 0usize;
    for byte in body.bytes() {
        if in_string {
            if escaped {
                escaped = false;
                string_bytes = string_bytes.saturating_add(1);
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'"' => {
                    in_string = false;
                    string_bytes = 0;
                }
                _ => {
                    string_bytes = string_bytes.saturating_add(1);
                    if string_bytes > MAX_BROWSER_PAGE_RECORDING_STRING_BYTES {
                        return Err(BrowserPageRecordingIpcError::Oversized);
                    }
                }
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string_count = string_count.saturating_add(1);
                if string_count > MAX_BROWSER_PAGE_RECORDING_IPC_STRINGS {
                    return Err(BrowserPageRecordingIpcError::TooManyItems);
                }
            }
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                container_count = container_count.saturating_add(1);
                if depth > MAX_BROWSER_PAGE_RECORDING_IPC_DEPTH {
                    return Err(BrowserPageRecordingIpcError::TooDeep);
                }
                if container_count > MAX_BROWSER_PAGE_RECORDING_IPC_CONTAINERS {
                    return Err(BrowserPageRecordingIpcError::TooManyItems);
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b':' => {
                member_count = member_count.saturating_add(1);
                if member_count > MAX_BROWSER_PAGE_RECORDING_IPC_MEMBERS {
                    return Err(BrowserPageRecordingIpcError::TooManyItems);
                }
            }
            _ => {}
        }
    }
    if in_string || depth != 0 {
        return Err(BrowserPageRecordingIpcError::Malformed);
    }
    Ok(())
}

struct StrictJsonValue(Value);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictValueVisitor;

        impl<'de> Visitor<'de> for StrictValueVisitor {
            type Value = StrictJsonValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON without duplicate object members")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .map(StrictJsonValue)
                    .ok_or_else(|| E::custom("JSON number must be finite"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::String(value.to_string())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::String(value)))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Null))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Null))
            }

            fn visit_some<D2>(self, deserializer: D2) -> Result<Self::Value, D2::Error>
            where
                D2: Deserializer<'de>,
            {
                StrictJsonValue::deserialize(deserializer)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
                    values.push(value.0);
                }
                Ok(StrictJsonValue(Value::Array(values)))
            }

            fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = object.next_key::<String>()? {
                    if values.contains_key(&key) {
                        let _ = object.next_value::<serde::de::IgnoredAny>()?;
                        return Err(A::Error::custom("duplicate JSON member"));
                    }
                    values.insert(key, object.next_value::<StrictJsonValue>()?.0);
                }
                Ok(StrictJsonValue(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(StrictValueVisitor)
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::browser::{BrowserRecordingCommit, BrowserWorkspaceKey};

    fn workspace() -> BrowserWorkspaceKey {
        BrowserWorkspaceKey {
            project_id: "project-a".to_string(),
            ai_tab_id: "ai-a".to_string(),
        }
    }

    fn click_body(instance_id: u64, sequence: u64, nonce: &str) -> String {
        format!(
            r##"{{"version":1,"channel":"browserRecording","workspace":{{"projectId":"project-a","aiTabId":"ai-a"}},"tabId":"tab-a","revision":7,"instanceId":{instance_id},"sequence":{sequence},"actor":"user","source":"page","origin":"https://example.test","event":{{"type":"click","locator":{{"accessibilityRole":"button","accessibilityName":"Save","testId":"save","cssSelectors":["#save"]}}}},"nonce":"{nonce}"}}"##
        )
    }

    #[test]
    fn transport_fences_then_drains_pre_stop_and_purges_stale_instances_on_restart() {
        let nonce_one = "11111111111111111111111111111111";
        let nonce_two = "22222222222222222222222222222222";
        let mut recorder = BrowserWorkflowRecorder::default();
        let first = recorder.start(workspace()).expect("first recording");
        let authority = BrowserPageRecordingAuthority::new(
            first.clone(),
            "tab-a",
            BrowserRevision(7),
            "https://example.test",
            nonce_one,
        )
        .expect("first authority");
        let mut ipc = BrowserPageRecordingIpc::default();
        ipc.activate(authority).expect("activate first IPC");
        let (transport, ingress) =
            browser_page_recording_transport(2, workspace(), "tab-a".to_string());
        ingress
            .activate(first.id(), nonce_one)
            .expect("activate ingress");

        assert_eq!(
            ingress.submit("https://example.test", click_body(first.id(), 0, nonce_one)),
            BrowserPageRecordingSubmit::Accepted
        );
        assert_eq!(
            ingress.fence(first.id(), nonce_one),
            BrowserPageRecordingSubmit::Fenced
        );
        let batch = transport.drain();
        assert!(batch.failures.is_empty());
        assert_eq!(batch.messages.len(), 1);
        for message in batch.messages {
            assert_eq!(
                ipc.ingest_from_origin(&mut recorder, &message.observed_origin, &message.body),
                Ok(BrowserRecordingCommit::Recorded)
            );
        }
        let review = recorder.stop(&first).expect("stop after accepted drain");
        assert_eq!(review.recipe().steps.len(), 1);
        assert_eq!(
            ingress.submit("https://example.test", click_body(first.id(), 1, nonce_one)),
            BrowserPageRecordingSubmit::Inactive,
            "events arriving after the synchronous fence never enter the queue"
        );
        assert!(transport.drain().messages.is_empty());
        recorder.discard(&first).expect("discard first review");

        let second = recorder.start(workspace()).expect("second recording");
        ingress
            .activate(second.id(), nonce_two)
            .expect("activate replacement");
        assert_eq!(
            ingress.submit("https://example.test", click_body(first.id(), 2, nonce_one)),
            BrowserPageRecordingSubmit::Stale,
            "old-instance page messages are rejected before consuming bounded capacity"
        );
        assert_eq!(
            ingress.submit(
                "https://example.test",
                click_body(second.id(), 0, nonce_two)
            ),
            BrowserPageRecordingSubmit::Accepted
        );
        let replacement_batch = transport.drain();
        assert_eq!(replacement_batch.messages.len(), 1);
        assert_eq!(replacement_batch.messages[0].instance_id, second.id());
    }

    #[test]
    fn transport_overflow_is_typed_and_invalidates_only_the_exact_incomplete_instance() {
        let nonce_one = "11111111111111111111111111111111";
        let nonce_two = "22222222222222222222222222222222";
        let (transport, ingress) =
            browser_page_recording_transport(1, workspace(), "tab-a".to_string());
        ingress
            .activate(41, nonce_one)
            .expect("activate first ingress");
        assert_eq!(
            ingress.submit("https://example.test", click_body(41, 0, nonce_one)),
            BrowserPageRecordingSubmit::Accepted
        );
        assert_eq!(
            ingress.submit("https://example.test", click_body(41, 1, nonce_one)),
            BrowserPageRecordingSubmit::Overflow
        );
        assert_eq!(
            ingress.submit("https://example.test", click_body(41, 2, nonce_one)),
            BrowserPageRecordingSubmit::Inactive,
            "the first transport failure closes the ingress before it can amplify failures"
        );
        let failed = transport.drain();
        assert_eq!(failed.messages.len(), 1);
        assert_eq!(failed.failures.len(), 1);
        assert_eq!(failed.failures[0].instance_id, 41);
        assert_eq!(
            failed.failures[0].kind,
            BrowserPageRecordingTransportFailureKind::Overflow
        );

        ingress
            .activate(42, nonce_two)
            .expect("activate replacement");
        assert_eq!(
            ingress.submit("https://example.test", click_body(41, 2, nonce_one)),
            BrowserPageRecordingSubmit::Stale
        );
        assert_eq!(
            ingress.submit("https://example.test", click_body(42, 0, nonce_two)),
            BrowserPageRecordingSubmit::Accepted,
            "stale traffic cannot refill or starve the replacement queue"
        );
        let replacement = transport.drain();
        assert!(replacement.failures.is_empty());
        assert_eq!(replacement.messages.len(), 1);
        assert_eq!(replacement.messages[0].instance_id, 42);
    }
}
