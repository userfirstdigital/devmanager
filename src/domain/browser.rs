//! Task-owned browser identities, facts, and bounded admission.
//!
//! This is the single domain contract. Host side-effects settle later; facts
//! never persist mutable COM handles or claim current pixels.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Bound;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::domain::artifact::PrivacyClass;
use crate::domain::id::{
    ArtifactId, BrowserContextId, BrowserRequestId, BrowserTabId, CommandId, OperationId,
    ResourceId, TaskId,
};

// Bound browser frames to the Phase 1 MessagePack physical/nesting/item caps.
const BROWSER_WIRE_MAX_BYTES: usize = 1024 * 1024;
const BROWSER_WIRE_MAX_DEPTH: u16 = 32;
const BROWSER_WIRE_MAX_ITEMS: u32 = 1_000;

pub const MAX_BROWSER_JOURNAL_FACTS: usize = 1_000;
pub const MAX_BROWSER_OPEN_TASKS: usize = 32;
pub const MAX_BROWSER_CONTEXTS: usize = 32;
pub const MAX_BROWSER_TABS: usize = 32;
pub const MAX_BROWSER_RECEIPTS: usize = 256;
pub const MAX_BROWSER_FACT_URL_BYTES: usize = 2_048;
pub const MAX_BROWSER_IDENTITY_BYTES: usize = 256;
pub const MAX_BROWSER_DIMENSION: u32 = 32_768;

/// Phase 8 host-surface dependencies that this domain slice cannot satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserIntegrationHold {
    WebViewSurfaceAbsent,
    BrowserServiceAbsent,
    HostCapabilityUngranted,
}

/// Typed HOLDs for absent Phase 8 host/runtime prerequisites.
pub fn browser_integration_holds() -> &'static [BrowserIntegrationHold] {
    &[
        BrowserIntegrationHold::WebViewSurfaceAbsent,
        BrowserIntegrationHold::BrowserServiceAbsent,
        BrowserIntegrationHold::HostCapabilityUngranted,
    ]
}

/// Exact identity a future BrowserService settler must present to complete an
/// accepted `Effect::HoldBrowserHost` operation.
///
/// Boundary (accepted HOLD → settler):
/// - command_id / operation_id / request_id / task_id / context_id / generation /
///   action_epoch are required and must match the durable receipt + effect.
/// - resource_id is required on the live host-surface bind path and must match
///   the registered native surface; the epoch-only `bind` path leaves it absent.
/// - `ReplayPolicy::NoAutomaticRetry` forbids first-attempt claim/dispatch.
/// - close/reopen must replay the same HOLD identity; it must not invent a settler.
/// - ClientModel pages stay bounded wire DTOs and are not a settle path.
/// - Legacy `browser::BrowserCommand` has none of these fields and cannot
///   construct or satisfy this intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserHostSettleIntent {
    command_id: CommandId,
    operation_id: OperationId,
    request_id: BrowserRequestId,
    task_id: TaskId,
    context_id: BrowserContextId,
    resource_id: Option<ResourceId>,
    generation: u64,
    action_epoch: u64,
}

impl BrowserHostSettleIntent {
    pub fn bind(
        command_id: CommandId,
        operation_id: OperationId,
        request_id: BrowserRequestId,
        task_id: TaskId,
        context_id: BrowserContextId,
        generation: u64,
        action_epoch: u64,
    ) -> Result<Self, BrowserContractError> {
        if generation == 0 {
            return Err(BrowserContractError::GenerationMismatch);
        }
        Ok(Self {
            command_id,
            operation_id,
            request_id,
            task_id,
            context_id,
            resource_id: None,
            generation,
            action_epoch,
        })
    }

    /// Live host-surface bind. Resource identity is required and cannot be
    /// inferred from task/context/generation alone.
    pub fn bind_host_surface(
        command_id: CommandId,
        operation_id: OperationId,
        request_id: BrowserRequestId,
        task_id: TaskId,
        context_id: BrowserContextId,
        resource_id: ResourceId,
        generation: u64,
        action_epoch: u64,
    ) -> Result<Self, BrowserContractError> {
        let mut intent = Self::bind(
            command_id,
            operation_id,
            request_id,
            task_id,
            context_id,
            generation,
            action_epoch,
        )?;
        intent.resource_id = Some(resource_id);
        Ok(intent)
    }

    pub fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub fn request_id(&self) -> BrowserRequestId {
        self.request_id
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn context_id(&self) -> BrowserContextId {
        self.context_id
    }

    pub fn resource_id(&self) -> Option<ResourceId> {
        self.resource_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    /// Exact accepted HOLD identity. Task, context, generation, request, and
    /// action_epoch must match the durable effect; generation stays nonzero.
    pub fn matches_accepted_hold(
        &self,
        task_id: TaskId,
        action_epoch: u64,
        request_id: BrowserRequestId,
        context_id: BrowserContextId,
        generation: u64,
    ) -> Result<(), BrowserContractError> {
        if self.task_id != task_id {
            return Err(BrowserContractError::CrossTask);
        }
        if generation == 0 || self.generation != generation {
            return Err(BrowserContractError::GenerationMismatch);
        }
        if self.context_id != context_id
            || self.request_id != request_id
            || self.action_epoch != action_epoch
        {
            return Err(BrowserContractError::InvalidRequest);
        }
        Ok(())
    }

    /// Exact registered surface identity for the live Windows settle path.
    pub fn matches_host_surface(
        &self,
        task_id: TaskId,
        context_id: BrowserContextId,
        resource_id: ResourceId,
        generation: u64,
    ) -> Result<(), BrowserContractError> {
        if self.task_id != task_id {
            return Err(BrowserContractError::CrossTask);
        }
        if generation == 0 || self.generation != generation {
            return Err(BrowserContractError::GenerationMismatch);
        }
        if self.context_id != context_id {
            return Err(BrowserContractError::InvalidRequest);
        }
        match self.resource_id {
            Some(expected) if expected == resource_id => Ok(()),
            Some(_) | None => Err(BrowserContractError::InvalidRequest),
        }
    }
}

mod settler_seal {
    pub trait Sealed {}
}

/// Only a granted BrowserService settler may complete an accepted host HOLD.
pub trait BrowserHostHoldSettler: settler_seal::Sealed {
    fn settle_accepted_hold(
        &self,
        intent: &BrowserHostSettleIntent,
    ) -> Result<BrowserHostOutcome, BrowserIntegrationHold>;
}

/// Proof the caller is the host-owned 8.3 `BrowserService`.
/// No public, `Default`, bool, or test constructor. Only that future service
/// may inhabit this type and then call `BrowserServiceAuthority::issue`.
pub struct BrowserServiceIssuer {
    #[allow(dead_code)]
    _private: (),
}

impl BrowserServiceIssuer {
    /// Host-owned 8.3 `BrowserService` is the only inhabitant of this proof.
    pub(crate) fn for_host_service() -> Self {
        Self { _private: () }
    }
}

/// Unforgeable 8.3 host authority. No public constructor. Crate-private
/// `issue` requires an uninhabited `BrowserServiceIssuer` plus the exact
/// host-owned surface identity observed on the live Windows path.
pub struct BrowserServiceAuthority {
    task_id: TaskId,
    context_id: BrowserContextId,
    resource_id: ResourceId,
    generation: u64,
}

impl BrowserServiceAuthority {
    /// Reserved for the host-owned 8.3 `BrowserService`. Not a public mint.
    pub(crate) fn issue(
        _issuer: &BrowserServiceIssuer,
        task_id: TaskId,
        context_id: BrowserContextId,
        resource_id: ResourceId,
        generation: u64,
    ) -> Result<Self, BrowserIntegrationHold> {
        if generation == 0 {
            return Err(BrowserIntegrationHold::WebViewSurfaceAbsent);
        }
        Ok(Self {
            task_id,
            context_id,
            resource_id,
            generation,
        })
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn context_id(&self) -> BrowserContextId {
        self.context_id
    }

    pub fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Production-constructible only from host-owned exact identity evidence plus
/// 8.3 authority. No public, `Default`, bool, or test constructor.
pub struct BrowserServiceSettlerToken {
    task_id: TaskId,
    context_id: BrowserContextId,
    resource_id: ResourceId,
    generation: u64,
    request_id: BrowserRequestId,
    action_epoch: u64,
}

impl BrowserServiceSettlerToken {
    pub(crate) fn from_host_owned_surface(
        authority: &BrowserServiceAuthority,
        intent: &BrowserHostSettleIntent,
    ) -> Result<Self, BrowserIntegrationHold> {
        let resource_id = intent
            .resource_id()
            .ok_or(BrowserIntegrationHold::WebViewSurfaceAbsent)?;
        intent
            .matches_host_surface(
                authority.task_id,
                authority.context_id,
                authority.resource_id,
                authority.generation,
            )
            .map_err(|_| BrowserIntegrationHold::WebViewSurfaceAbsent)?;
        if resource_id != authority.resource_id {
            return Err(BrowserIntegrationHold::WebViewSurfaceAbsent);
        }
        Ok(Self {
            task_id: authority.task_id,
            context_id: authority.context_id,
            resource_id,
            generation: authority.generation,
            request_id: intent.request_id(),
            action_epoch: intent.action_epoch(),
        })
    }
}

impl settler_seal::Sealed for BrowserServiceSettlerToken {}

impl BrowserHostHoldSettler for BrowserServiceSettlerToken {
    fn settle_accepted_hold(
        &self,
        intent: &BrowserHostSettleIntent,
    ) -> Result<BrowserHostOutcome, BrowserIntegrationHold> {
        intent
            .matches_accepted_hold(
                self.task_id,
                self.action_epoch,
                self.request_id,
                self.context_id,
                self.generation,
            )
            .map_err(|_| BrowserIntegrationHold::WebViewSurfaceAbsent)?;
        intent
            .matches_host_surface(
                self.task_id,
                self.context_id,
                self.resource_id,
                self.generation,
            )
            .map_err(|_| BrowserIntegrationHold::WebViewSurfaceAbsent)?;
        Ok(BrowserHostOutcome {
            request_id: intent.request_id(),
            task_id: self.task_id,
            context_id: self.context_id,
            tab_id: None,
            generation: intent.generation(),
            settlement: BrowserSettlement::Recovered {
                generation: intent.generation(),
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserPermission {
    CreateContext,
    CloseContext,
    OpenTab,
    CloseTab,
    SelectTab,
    Navigate,
    History,
    SetBounds,
    SetVisibility,
    SetFocus,
    Capture,
    Automate,
    Download,
    Clipboard,
    FileChooser,
    SecretFill,
    PermissionDecide,
    Record,
    Replay,
    Cancel,
    Recover,
    LinkArtifact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserHealth {
    Healthy,
    Recovering,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTabKind {
    Page,
    Popup { opener: BrowserTabId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserContractError {
    CrossTask,
    GenerationMismatch,
    ClosedTask,
    IdempotencyConflict,
    BoundExceeded,
    InvalidRequest,
    HostEffectUnavailable,
}

impl fmt::Display for BrowserContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CrossTask => write!(f, "browser identity belongs to a different Task"),
            Self::GenerationMismatch => write!(f, "browser request generation does not match"),
            Self::ClosedTask => write!(f, "closed Task cannot admit browser work"),
            Self::IdempotencyConflict => {
                write!(f, "request id was reused with a different payload")
            }
            Self::BoundExceeded => write!(f, "browser contract bound exceeded"),
            Self::InvalidRequest => write!(f, "browser request is not admissible"),
            Self::HostEffectUnavailable => {
                write!(
                    f,
                    "browser host effect is unavailable until the WebView2 surface exists"
                )
            }
        }
    }
}

impl std::error::Error for BrowserContractError {}

fn deserialize_identity<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    validate_identity(&value).map_err(de::Error::custom)?;
    Ok(value)
}

fn deserialize_url<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    validate_url(&value).map_err(de::Error::custom)?;
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserDurableFact {
    RequestAccepted {
        request_id: BrowserRequestId,
        task_id: TaskId,
        context_id: BrowserContextId,
        tab_id: Option<BrowserTabId>,
        generation: u64,
        action: BrowserAction,
        privacy_class: PrivacyClass,
        permission: BrowserPermission,
        payload_hash: [u8; 32],
        action_epoch: u64,
        command_id: Option<CommandId>,
    },
    ContextCreated {
        context_id: BrowserContextId,
        task_id: TaskId,
        generation: u64,
    },
    ContextClosed {
        context_id: BrowserContextId,
        task_id: TaskId,
        generation: u64,
    },
    TabOpened {
        tab_id: BrowserTabId,
        context_id: BrowserContextId,
        task_id: TaskId,
        kind: BrowserTabKind,
        #[serde(deserialize_with = "deserialize_url")]
        url: String,
    },
    TabClosed {
        tab_id: BrowserTabId,
        context_id: BrowserContextId,
        task_id: TaskId,
    },
    TabSelected {
        tab_id: BrowserTabId,
        context_id: BrowserContextId,
        task_id: TaskId,
    },
    NavigationCommitted {
        tab_id: BrowserTabId,
        context_id: BrowserContextId,
        task_id: TaskId,
        #[serde(deserialize_with = "deserialize_url")]
        url: String,
        #[serde(deserialize_with = "deserialize_identity")]
        document_id: String,
    },
    PermissionDecided {
        context_id: BrowserContextId,
        task_id: TaskId,
        permission: BrowserPermission,
        allowed: bool,
    },
    ArtifactLinked {
        context_id: BrowserContextId,
        task_id: TaskId,
        artifact_id: ArtifactId,
    },
    RecipeIdentified {
        context_id: BrowserContextId,
        task_id: TaskId,
        #[serde(deserialize_with = "deserialize_identity")]
        recipe_id: String,
    },
    RecordingIdentified {
        context_id: BrowserContextId,
        task_id: TaskId,
        #[serde(deserialize_with = "deserialize_identity")]
        recording_id: String,
    },
    DownloadSettled {
        context_id: BrowserContextId,
        task_id: TaskId,
        allowed: bool,
        #[serde(deserialize_with = "deserialize_identity")]
        file_name: String,
        sha256_hex: Option<String>,
        artifact_id: Option<ArtifactId>,
    },
    HealthTransitioned {
        context_id: BrowserContextId,
        task_id: TaskId,
        from: BrowserHealth,
        to: BrowserHealth,
        generation: u64,
    },
}

impl BrowserDurableFact {
    pub fn claims_ephemeral_runtime(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserAction {
    CreateContext,
    CloseContext,
    OpenTab {
        #[serde(deserialize_with = "deserialize_url")]
        url: String,
        kind: BrowserTabKind,
    },
    CloseTab,
    SelectTab,
    Navigate {
        #[serde(deserialize_with = "deserialize_url")]
        url: String,
    },
    Back,
    Forward,
    Reload,
    Stop,
    SetBounds {
        width: u32,
        height: u32,
    },
    SetVisibility {
        visible: bool,
    },
    SetFocus {
        focused: bool,
    },
    Capture,
    Automate,
    DecideDownload {
        allow: bool,
    },
    ReadClipboard,
    WriteClipboard,
    ChooseFile {
        artifact_id: ArtifactId,
    },
    FillSecret {
        #[serde(deserialize_with = "deserialize_identity")]
        vault_ref: String,
        #[serde(deserialize_with = "deserialize_identity")]
        field_selector: String,
    },
    DecidePermission {
        permission: BrowserPermission,
        allowed: bool,
    },
    Record,
    Replay,
    Cancel,
    Recover,
    LinkArtifact {
        artifact_id: ArtifactId,
    },
}

impl BrowserAction {
    pub fn privacy_class(&self) -> PrivacyClass {
        match self {
            Self::OpenTab { .. }
            | Self::Navigate { .. }
            | Self::Back
            | Self::Forward
            | Self::Reload
            | Self::Stop
            | Self::Capture
            | Self::Automate
            | Self::DecideDownload { .. }
            | Self::ReadClipboard
            | Self::WriteClipboard
            | Self::FillSecret { .. }
            | Self::Record
            | Self::Replay => PrivacyClass::LocalOnly,
            _ => PrivacyClass::Shareable,
        }
    }

    pub fn required_permission(&self) -> BrowserPermission {
        match self {
            Self::CreateContext => BrowserPermission::CreateContext,
            Self::CloseContext => BrowserPermission::CloseContext,
            Self::OpenTab { .. } => BrowserPermission::OpenTab,
            Self::CloseTab => BrowserPermission::CloseTab,
            Self::SelectTab => BrowserPermission::SelectTab,
            Self::Navigate { .. } => BrowserPermission::Navigate,
            Self::Back | Self::Forward | Self::Reload | Self::Stop => BrowserPermission::History,
            Self::SetBounds { .. } => BrowserPermission::SetBounds,
            Self::SetVisibility { .. } => BrowserPermission::SetVisibility,
            Self::SetFocus { .. } => BrowserPermission::SetFocus,
            Self::Capture => BrowserPermission::Capture,
            Self::Automate => BrowserPermission::Automate,
            Self::DecideDownload { .. } => BrowserPermission::Download,
            Self::ReadClipboard | Self::WriteClipboard => BrowserPermission::Clipboard,
            Self::ChooseFile { .. } => BrowserPermission::FileChooser,
            Self::FillSecret { .. } => BrowserPermission::SecretFill,
            Self::DecidePermission { .. } => BrowserPermission::PermissionDecide,
            Self::Record => BrowserPermission::Record,
            Self::Replay => BrowserPermission::Replay,
            Self::Cancel => BrowserPermission::Cancel,
            Self::Recover => BrowserPermission::Recover,
            Self::LinkArtifact { .. } => BrowserPermission::LinkArtifact,
        }
    }

    fn url(&self) -> Option<&str> {
        match self {
            Self::OpenTab { url, .. } | Self::Navigate { url } => Some(url),
            _ => None,
        }
    }

    pub(crate) fn requires_host_settlement(&self) -> bool {
        matches!(
            self,
            Self::Navigate { .. }
                | Self::Back
                | Self::Forward
                | Self::Reload
                | Self::Stop
                | Self::SetBounds { .. }
                | Self::SetVisibility { .. }
                | Self::SetFocus { .. }
                | Self::Capture
                | Self::Automate
                | Self::DecideDownload { .. }
                | Self::ReadClipboard
                | Self::WriteClipboard
                | Self::ChooseFile { .. }
                | Self::FillSecret { .. }
                | Self::DecidePermission { .. }
                | Self::Record
                | Self::Replay
                | Self::Cancel
                | Self::Recover
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserRequest {
    pub request_id: BrowserRequestId,
    pub task_id: TaskId,
    pub context_id: BrowserContextId,
    pub tab_id: Option<BrowserTabId>,
    pub generation: u64,
    pub action: BrowserAction,
}

impl<'de> Deserialize<'de> for BrowserRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RequestVisitor;
        impl<'de> Visitor<'de> for RequestVisitor {
            type Value = BrowserRequest;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a bounded BrowserRequest")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut request_id = None;
                let mut task_id = None;
                let mut context_id = None;
                let mut tab_id = None;
                let mut seen_tab_id = false;
                let mut generation = None;
                let mut action = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "request_id" if request_id.is_none() => {
                            request_id = Some(map.next_value()?);
                        }
                        "task_id" if task_id.is_none() => task_id = Some(map.next_value()?),
                        "context_id" if context_id.is_none() => {
                            context_id = Some(map.next_value()?);
                        }
                        "tab_id" if !seen_tab_id => {
                            seen_tab_id = true;
                            tab_id = Some(map.next_value()?);
                        }
                        "generation" if generation.is_none() => {
                            generation = Some(map.next_value()?);
                        }
                        "action" if action.is_none() => action = Some(map.next_value()?),
                        "request_id" | "task_id" | "context_id" | "tab_id" | "generation"
                        | "action" => {
                            return Err(de::Error::custom("duplicate browser request field"));
                        }
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &[
                                    "request_id",
                                    "task_id",
                                    "context_id",
                                    "tab_id",
                                    "generation",
                                    "action",
                                ],
                            ));
                        }
                    }
                }
                Ok(BrowserRequest {
                    request_id: request_id.ok_or_else(|| de::Error::missing_field("request_id"))?,
                    task_id: task_id.ok_or_else(|| de::Error::missing_field("task_id"))?,
                    context_id: context_id.ok_or_else(|| de::Error::missing_field("context_id"))?,
                    tab_id: tab_id.unwrap_or(None),
                    generation: generation.ok_or_else(|| de::Error::missing_field("generation"))?,
                    action: action.ok_or_else(|| de::Error::missing_field("action"))?,
                })
            }
        }
        deserializer.deserialize_map(RequestVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserAcceptedReceipt {
    pub request_id: BrowserRequestId,
    pub command_id: Option<CommandId>,
    pub task_id: TaskId,
    pub context_id: BrowserContextId,
    pub tab_id: Option<BrowserTabId>,
    pub generation: u64,
    pub action: BrowserAction,
    pub facts: Vec<BrowserDurableFact>,
    pub privacy_class: PrivacyClass,
    pub permission: BrowserPermission,
    pub payload_hash: [u8; 32],
    pub action_epoch: u64,
}

impl BrowserAcceptedReceipt {
    pub(crate) fn bind_command(&mut self, command_id: CommandId, action_epoch: u64) {
        self.command_id = Some(command_id);
        self.action_epoch = action_epoch;
        for fact in &mut self.facts {
            if let BrowserDurableFact::RequestAccepted {
                command_id: bound_command,
                action_epoch: bound_epoch,
                ..
            } = fact
            {
                *bound_command = Some(command_id);
                *bound_epoch = action_epoch;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserHostOutcome {
    pub request_id: BrowserRequestId,
    pub task_id: TaskId,
    pub context_id: BrowserContextId,
    pub tab_id: Option<BrowserTabId>,
    pub generation: u64,
    pub settlement: BrowserSettlement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserSettlement {
    NavigationCommitted {
        url: String,
        document_id: String,
    },
    PermissionDecided {
        permission: BrowserPermission,
        allowed: bool,
    },
    Recovered {
        generation: u64,
    },
    RecipeIdentified {
        recipe_id: String,
    },
    RecordingIdentified {
        recording_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserContextView {
    pub context_id: BrowserContextId,
    pub task_id: TaskId,
    pub generation: u64,
    pub selected_tab_id: Option<BrowserTabId>,
    pub health: BrowserHealth,
    pub closed: bool,
    pub permissions: BTreeMap<BrowserPermission, bool>,
    pub linked_artifacts: BTreeSet<ArtifactId>,
    pub recipe_id: Option<String>,
    pub recording_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserTabView {
    pub tab_id: BrowserTabId,
    pub context_id: BrowserContextId,
    pub task_id: TaskId,
    pub kind: BrowserTabKind,
    pub committed_url: Option<String>,
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserIdentitySnapshot {
    pub contexts: Vec<BrowserContextView>,
    pub tabs: Vec<BrowserTabView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSnapshotSection {
    Contexts,
    Tabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserPageKey {
    Context(BrowserContextId),
    Tab(BrowserTabId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum BrowserSnapshotRow {
    Context(BrowserContextView),
    Tab(BrowserTabView),
}

impl BrowserSnapshotRow {
    pub fn task_id(&self) -> Option<TaskId> {
        match self {
            Self::Context(view) => Some(view.task_id),
            Self::Tab(view) => Some(view.task_id),
        }
    }

    pub fn tab_id(&self) -> Option<BrowserTabId> {
        match self {
            Self::Tab(view) => Some(view.tab_id),
            Self::Context(_) => None,
        }
    }

    pub fn generation(&self) -> Option<u64> {
        match self {
            Self::Context(view) => Some(view.generation),
            Self::Tab(_) => None,
        }
    }

    pub fn closed(&self) -> bool {
        match self {
            Self::Context(view) => view.closed,
            Self::Tab(view) => view.closed,
        }
    }

    pub fn shareable_url(&self) -> Option<String> {
        match self {
            Self::Tab(view) => view.committed_url.as_deref().and_then(shareable_origin),
            Self::Context(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSnapshotPage {
    pub section: BrowserSnapshotSection,
    pub after_item: Option<BrowserPageKey>,
    pub items: Vec<BrowserSnapshotRow>,
    pub next_after: Option<BrowserPageKey>,
    pub next_cursor: Option<Vec<u8>>,
    pub examined: usize,
    pub encoded_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextState {
    task_id: TaskId,
    generation: u64,
    selected_tab_id: Option<BrowserTabId>,
    health: BrowserHealth,
    closed: bool,
    permissions: BTreeMap<BrowserPermission, bool>,
    linked_artifacts: BTreeSet<ArtifactId>,
    recipe_id: Option<String>,
    recording_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TabState {
    context_id: BrowserContextId,
    task_id: TaskId,
    kind: BrowserTabKind,
    committed_url: Option<String>,
    closed: bool,
}

impl ContextState {
    fn view(&self, context_id: BrowserContextId) -> BrowserContextView {
        BrowserContextView {
            context_id,
            task_id: self.task_id,
            generation: self.generation,
            selected_tab_id: self.selected_tab_id,
            health: self.health,
            closed: self.closed,
            permissions: self.permissions.clone(),
            linked_artifacts: self.linked_artifacts.clone(),
            recipe_id: self.recipe_id.clone(),
            recording_id: self.recording_id.clone(),
        }
    }
}

impl TabState {
    fn view(&self, tab_id: BrowserTabId) -> BrowserTabView {
        BrowserTabView {
            tab_id,
            context_id: self.context_id,
            task_id: self.task_id,
            kind: self.kind,
            committed_url: self.committed_url.clone(),
            closed: self.closed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredReceipt {
    request: BrowserRequest,
    accepted: BrowserAcceptedReceipt,
    settled: bool,
    settlement: Option<BrowserSettlement>,
}

#[derive(Debug, Clone)]
struct Projection {
    contexts: BTreeMap<BrowserContextId, ContextState>,
    tabs: BTreeMap<BrowserTabId, TabState>,
    facts: Vec<BrowserDurableFact>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowserBook {
    ready: bool,
    open_tasks: BTreeSet<TaskId>,
    contexts: BTreeMap<BrowserContextId, ContextState>,
    tabs: BTreeMap<BrowserTabId, TabState>,
    facts: Vec<BrowserDurableFact>,
    receipts: BTreeMap<BrowserRequestId, StoredReceipt>,
}

impl BrowserBook {
    pub fn new() -> Self {
        Self {
            ready: true,
            ..Self::default()
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn untrusted() -> Self {
        Self::default()
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn context_view(&self, context_id: BrowserContextId) -> Option<BrowserContextView> {
        self.contexts
            .get(&context_id)
            .map(|context| context.view(context_id))
    }

    pub fn tab_view(&self, tab_id: BrowserTabId) -> Option<BrowserTabView> {
        self.tabs.get(&tab_id).map(|tab| tab.view(tab_id))
    }

    /// Return the bounded browser identity projection retained by this book.
    ///
    /// Client projections use this instead of reaching into the private
    /// context/tab maps, keeping browser identity assembly in the domain
    /// boundary while preserving the durable generation and selected tab.
    pub fn identity_snapshot(&self) -> BrowserIdentitySnapshot {
        BrowserIdentitySnapshot {
            contexts: self
                .contexts
                .iter()
                .map(|(id, context)| context.view(*id))
                .collect(),
            tabs: self
                .tabs
                .iter()
                .map(|(id, tab)| tab.view(*id))
                .collect(),
        }
    }

    pub(crate) fn project_context_view(
        &mut self,
        view: &BrowserContextView,
    ) -> Result<(), BrowserContractError> {
        self.open_task(view.task_id)?;
        if self.contexts.contains_key(&view.context_id) {
            return Err(BrowserContractError::InvalidRequest);
        }
        if self.contexts.len() >= MAX_BROWSER_CONTEXTS {
            return Err(BrowserContractError::BoundExceeded);
        }
        self.contexts.insert(
            view.context_id,
            ContextState {
                task_id: view.task_id,
                generation: view.generation,
                selected_tab_id: view.selected_tab_id,
                health: view.health,
                closed: view.closed,
                permissions: view.permissions.clone(),
                linked_artifacts: view.linked_artifacts.clone(),
                recipe_id: view.recipe_id.clone(),
                recording_id: view.recording_id.clone(),
            },
        );
        Ok(())
    }

    pub(crate) fn project_tab_view(
        &mut self,
        view: &BrowserTabView,
    ) -> Result<(), BrowserContractError> {
        self.open_task(view.task_id)?;
        let Some(context) = self.contexts.get(&view.context_id) else {
            return Err(BrowserContractError::InvalidRequest);
        };
        if context.task_id != view.task_id || self.tabs.contains_key(&view.tab_id) {
            return Err(BrowserContractError::InvalidRequest);
        }
        if self.tabs.len() >= MAX_BROWSER_TABS {
            return Err(BrowserContractError::BoundExceeded);
        }
        self.tabs.insert(
            view.tab_id,
            TabState {
                context_id: view.context_id,
                task_id: view.task_id,
                kind: view.kind,
                committed_url: view.committed_url.clone(),
                closed: view.closed,
            },
        );
        Ok(())
    }

    pub fn open_task(&mut self, task_id: TaskId) -> Result<(), BrowserContractError> {
        if self.open_tasks.contains(&task_id) {
            return Ok(());
        }
        if self.open_tasks.len() >= MAX_BROWSER_OPEN_TASKS {
            return Err(BrowserContractError::BoundExceeded);
        }
        self.open_tasks.insert(task_id);
        Ok(())
    }

    pub fn close_task(&mut self, task_id: TaskId) -> Result<(), BrowserContractError> {
        if !self.open_tasks.contains(&task_id) {
            return Err(BrowserContractError::ClosedTask);
        }
        let mut facts = Vec::new();
        for (tab_id, tab) in &self.tabs {
            if tab.task_id == task_id && !tab.closed {
                facts.push(BrowserDurableFact::TabClosed {
                    tab_id: *tab_id,
                    context_id: tab.context_id,
                    task_id,
                });
            }
        }
        for (context_id, context) in &self.contexts {
            if context.task_id == task_id && !context.closed {
                facts.push(BrowserDurableFact::ContextClosed {
                    context_id: *context_id,
                    task_id,
                    generation: context.generation,
                });
            }
        }
        self.apply_facts(&facts)?;
        self.open_tasks.remove(&task_id);
        Ok(())
    }

    pub fn facts(&self) -> &[BrowserDurableFact] {
        &self.facts
    }

    pub fn receipt_count(&self) -> usize {
        self.receipts.len()
    }

    pub fn snapshot_page(
        &self,
        section: BrowserSnapshotSection,
        after: Option<BrowserPageKey>,
        max_items: u32,
        max_encoded_bytes: u32,
    ) -> Result<BrowserSnapshotPage, BrowserContractError> {
        if max_items == 0 || max_encoded_bytes == 0 {
            return Err(BrowserContractError::BoundExceeded);
        }
        let take = max_items as usize;
        let mut items = Vec::new();
        let mut examined = 0;
        let mut next_after = None;
        match section {
            BrowserSnapshotSection::Contexts => {
                let start = match after {
                    Some(BrowserPageKey::Context(id)) => Bound::Excluded(id),
                    None => Bound::Unbounded,
                    Some(_) => return Err(BrowserContractError::InvalidRequest),
                };
                for (id, context) in self.contexts.range((start, Bound::Unbounded)) {
                    examined += 1;
                    if items.len() == take {
                        next_after = Some(BrowserPageKey::Context(*id));
                        break;
                    }
                    let row = BrowserSnapshotRow::Context(context.view(*id));
                    if page_encoded_bytes(&items, &row)? > max_encoded_bytes {
                        if items.is_empty() {
                            return Err(BrowserContractError::BoundExceeded);
                        }
                        next_after = Some(BrowserPageKey::Context(*id));
                        break;
                    }
                    items.push(row);
                }
            }
            BrowserSnapshotSection::Tabs => {
                let start = match after {
                    Some(BrowserPageKey::Tab(id)) => Bound::Excluded(id),
                    None => Bound::Unbounded,
                    Some(_) => return Err(BrowserContractError::InvalidRequest),
                };
                for (id, tab) in self.tabs.range((start, Bound::Unbounded)) {
                    examined += 1;
                    if items.len() == take {
                        next_after = Some(BrowserPageKey::Tab(*id));
                        break;
                    }
                    let row = BrowserSnapshotRow::Tab(tab.view(*id));
                    if page_encoded_bytes(&items, &row)? > max_encoded_bytes {
                        if items.is_empty() {
                            return Err(BrowserContractError::BoundExceeded);
                        }
                        next_after = Some(BrowserPageKey::Tab(*id));
                        break;
                    }
                    items.push(row);
                }
            }
        }
        let encoded_bytes = match rmp_serde::to_vec_named(&items) {
            Ok(bytes) => u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            Err(_) => return Err(BrowserContractError::BoundExceeded),
        };
        Ok(BrowserSnapshotPage {
            section,
            after_item: after,
            items,
            next_after,
            next_cursor: next_after.map(key_bytes),
            examined,
            encoded_bytes,
        })
    }

    pub fn plan_admit(
        &self,
        request: &BrowserRequest,
    ) -> Result<BrowserAcceptedReceipt, BrowserContractError> {
        if !self.ready {
            return Err(BrowserContractError::HostEffectUnavailable);
        }
        if let Some(prior) = self.receipts.get(&request.request_id) {
            if prior.request == *request {
                let mut replay = prior.accepted.clone();
                replay
                    .facts
                    .retain(|fact| matches!(fact, BrowserDurableFact::RequestAccepted { .. }));
                if replay.facts.is_empty() {
                    replay.facts.push(BrowserDurableFact::RequestAccepted {
                        request_id: replay.request_id,
                        task_id: replay.task_id,
                        context_id: replay.context_id,
                        tab_id: replay.tab_id,
                        generation: replay.generation,
                        action: replay.action.clone(),
                        privacy_class: replay.privacy_class,
                        permission: replay.permission,
                        payload_hash: replay.payload_hash,
                        action_epoch: replay.action_epoch,
                        command_id: replay.command_id,
                    });
                }
                return Ok(replay);
            }
            return Err(BrowserContractError::IdempotencyConflict);
        }
        if !self.open_tasks.contains(&request.task_id) {
            return Err(BrowserContractError::ClosedTask);
        }
        if self.receipts.len() >= MAX_BROWSER_RECEIPTS {
            return Err(BrowserContractError::BoundExceeded);
        }
        if request.generation == 0 {
            return Err(BrowserContractError::GenerationMismatch);
        }
        if let Some(url) = request.action.url() {
            validate_url(url)?;
        }
        if let BrowserAction::FillSecret {
            vault_ref,
            field_selector,
        } = &request.action
        {
            validate_identity(vault_ref)?;
            validate_identity(field_selector)?;
        }
        if let BrowserAction::SetBounds { width, height } = request.action {
            if width == 0
                || height == 0
                || width > MAX_BROWSER_DIMENSION
                || height > MAX_BROWSER_DIMENSION
            {
                return Err(BrowserContractError::InvalidRequest);
            }
        }
        let identity = self.plan_identity_facts(request)?;
        if self
            .facts
            .len()
            .saturating_add(identity.len().saturating_add(1))
            > MAX_BROWSER_JOURNAL_FACTS
        {
            return Err(BrowserContractError::BoundExceeded);
        }
        let payload_hash = request_payload_hash(request);
        let accepted_fact = BrowserDurableFact::RequestAccepted {
            request_id: request.request_id,
            task_id: request.task_id,
            context_id: request.context_id,
            tab_id: request.tab_id,
            generation: request.generation,
            action: request.action.clone(),
            privacy_class: request.action.privacy_class(),
            permission: request.action.required_permission(),
            payload_hash,
            action_epoch: 0,
            command_id: None,
        };
        let mut facts = vec![accepted_fact];
        facts.extend(identity);
        Ok(BrowserAcceptedReceipt {
            request_id: request.request_id,
            command_id: None,
            task_id: request.task_id,
            context_id: request.context_id,
            tab_id: request.tab_id,
            generation: request.generation,
            action: request.action.clone(),
            facts,
            privacy_class: request.action.privacy_class(),
            permission: request.action.required_permission(),
            payload_hash,
            action_epoch: 0,
        })
    }

    pub fn admit(
        &mut self,
        request: &BrowserRequest,
    ) -> Result<BrowserAcceptedReceipt, BrowserContractError> {
        let accepted = self.plan_admit(request)?;
        if let Some(prior) = self.receipts.get(&request.request_id) {
            let mut returned = prior.accepted.clone();
            returned
                .facts
                .retain(|fact| !matches!(fact, BrowserDurableFact::RequestAccepted { .. }));
            return Ok(returned);
        }
        self.apply_facts(&accepted.facts)?;
        self.receipts.insert(
            request.request_id,
            StoredReceipt {
                request: request.clone(),
                accepted: accepted.clone(),
                settled: !request.action.requires_host_settlement(),
                settlement: None,
            },
        );
        let mut returned = accepted;
        returned
            .facts
            .retain(|fact| !matches!(fact, BrowserDurableFact::RequestAccepted { .. }));
        Ok(returned)
    }

    pub fn settle(
        &mut self,
        outcome: &BrowserHostOutcome,
    ) -> Result<BrowserAcceptedReceipt, BrowserContractError> {
        let receipt = self
            .receipts
            .get(&outcome.request_id)
            .ok_or(BrowserContractError::InvalidRequest)?;
        if receipt.request.task_id != outcome.task_id
            || receipt.request.context_id != outcome.context_id
            || receipt.request.tab_id != outcome.tab_id
            || receipt.request.generation != outcome.generation
        {
            return Err(BrowserContractError::InvalidRequest);
        }
        if receipt.settled {
            if receipt.settlement.as_ref() == Some(&outcome.settlement) {
                return Ok(receipt.accepted.clone());
            }
            return Err(BrowserContractError::IdempotencyConflict);
        }
        let facts = match (&receipt.request.action, &outcome.settlement) {
            (
                BrowserAction::Navigate { .. } | BrowserAction::Reload,
                BrowserSettlement::NavigationCommitted { url, document_id },
            ) => {
                validate_url(url)?;
                validate_identity(document_id)?;
                vec![BrowserDurableFact::NavigationCommitted {
                    tab_id: outcome.tab_id.ok_or(BrowserContractError::InvalidRequest)?,
                    context_id: outcome.context_id,
                    task_id: outcome.task_id,
                    url: url.clone(),
                    document_id: document_id.clone(),
                }]
            }
            (
                BrowserAction::DecidePermission { .. } | BrowserAction::DecideDownload { .. },
                BrowserSettlement::PermissionDecided {
                    permission,
                    allowed,
                },
            ) => vec![BrowserDurableFact::PermissionDecided {
                context_id: outcome.context_id,
                task_id: outcome.task_id,
                permission: *permission,
                allowed: *allowed,
            }],
            (BrowserAction::Recover, BrowserSettlement::Recovered { generation }) => {
                let context = self.require_context_ids(
                    outcome.task_id,
                    outcome.context_id,
                    outcome.generation,
                )?;
                vec![BrowserDurableFact::HealthTransitioned {
                    context_id: outcome.context_id,
                    task_id: outcome.task_id,
                    from: context.health,
                    to: BrowserHealth::Recovering,
                    generation: *generation,
                }]
            }
            (BrowserAction::Replay, BrowserSettlement::RecipeIdentified { recipe_id }) => {
                validate_identity(recipe_id)?;
                vec![BrowserDurableFact::RecipeIdentified {
                    context_id: outcome.context_id,
                    task_id: outcome.task_id,
                    recipe_id: recipe_id.clone(),
                }]
            }
            (BrowserAction::Record, BrowserSettlement::RecordingIdentified { recording_id }) => {
                validate_identity(recording_id)?;
                vec![BrowserDurableFact::RecordingIdentified {
                    context_id: outcome.context_id,
                    task_id: outcome.task_id,
                    recording_id: recording_id.clone(),
                }]
            }
            _ => return Err(BrowserContractError::InvalidRequest),
        };
        self.apply_facts(&facts)?;
        let mut accepted = self
            .receipts
            .get(&outcome.request_id)
            .expect("receipt exists")
            .accepted
            .clone();
        accepted.facts = facts;
        if let Some(stored) = self.receipts.get_mut(&outcome.request_id) {
            stored.settled = true;
            stored.settlement = Some(outcome.settlement.clone());
        }
        Ok(accepted)
    }

    pub fn plan_close_task(&self, task_id: TaskId) -> Vec<BrowserDurableFact> {
        let mut facts = Vec::new();
        for (tab_id, tab) in &self.tabs {
            if tab.task_id == task_id && !tab.closed {
                facts.push(BrowserDurableFact::TabClosed {
                    tab_id: *tab_id,
                    context_id: tab.context_id,
                    task_id,
                });
            }
        }
        for (context_id, context) in &self.contexts {
            if context.task_id == task_id && !context.closed {
                facts.push(BrowserDurableFact::ContextClosed {
                    context_id: *context_id,
                    task_id,
                    generation: context.generation,
                });
            }
        }
        facts
    }

    pub fn apply_facts(
        &mut self,
        facts: &[BrowserDurableFact],
    ) -> Result<(), BrowserContractError> {
        if facts.is_empty() {
            return Ok(());
        }
        if self.facts.len().saturating_add(facts.len()) > MAX_BROWSER_JOURNAL_FACTS {
            return Err(BrowserContractError::BoundExceeded);
        }
        let backup = Projection {
            contexts: self.contexts.clone(),
            tabs: self.tabs.clone(),
            facts: self.facts.clone(),
        };
        let receipt_backup = self.receipts.clone();
        for fact in facts {
            if let Err(error) = self.apply_fact(fact) {
                self.contexts = backup.contexts;
                self.tabs = backup.tabs;
                self.facts = backup.facts;
                self.receipts = receipt_backup;
                return Err(error);
            }
        }
        Ok(())
    }

    fn plan_identity_facts(
        &self,
        request: &BrowserRequest,
    ) -> Result<Vec<BrowserDurableFact>, BrowserContractError> {
        match &request.action {
            BrowserAction::CreateContext => {
                if request.generation != 1 {
                    return Err(BrowserContractError::GenerationMismatch);
                }
                if let Some(existing) = self.contexts.get(&request.context_id) {
                    return Err(if existing.task_id == request.task_id {
                        BrowserContractError::InvalidRequest
                    } else {
                        BrowserContractError::CrossTask
                    });
                }
                if self.contexts.len() >= MAX_BROWSER_CONTEXTS {
                    return Err(BrowserContractError::BoundExceeded);
                }
                Ok(vec![BrowserDurableFact::ContextCreated {
                    context_id: request.context_id,
                    task_id: request.task_id,
                    generation: 1,
                }])
            }
            BrowserAction::CloseContext => {
                if let Some(existing) = self.contexts.get(&request.context_id) {
                    if existing.task_id != request.task_id {
                        return Err(BrowserContractError::CrossTask);
                    }
                    if existing.closed {
                        return Ok(Vec::new());
                    }
                }
                let context = self.require_context(request)?;
                let mut facts = Vec::new();
                for (tab_id, tab) in &self.tabs {
                    if tab.context_id == request.context_id && !tab.closed {
                        facts.push(BrowserDurableFact::TabClosed {
                            tab_id: *tab_id,
                            context_id: request.context_id,
                            task_id: request.task_id,
                        });
                    }
                }
                facts.push(BrowserDurableFact::ContextClosed {
                    context_id: request.context_id,
                    task_id: request.task_id,
                    generation: context.generation,
                });
                Ok(facts)
            }
            BrowserAction::OpenTab { url, kind } => {
                let context = self.require_context(request)?;
                let tab_id = request.tab_id.ok_or(BrowserContractError::InvalidRequest)?;
                if self.tabs.contains_key(&tab_id) {
                    return Err(BrowserContractError::InvalidRequest);
                }
                if self.tabs.values().filter(|tab| !tab.closed).count() >= MAX_BROWSER_TABS {
                    return Err(BrowserContractError::BoundExceeded);
                }
                if let BrowserTabKind::Popup { opener } = kind {
                    let parent = self
                        .tabs
                        .get(opener)
                        .ok_or(BrowserContractError::InvalidRequest)?;
                    if parent.closed
                        || parent.context_id != request.context_id
                        || parent.task_id != request.task_id
                    {
                        return Err(BrowserContractError::InvalidRequest);
                    }
                }
                let mut facts = vec![BrowserDurableFact::TabOpened {
                    tab_id,
                    context_id: request.context_id,
                    task_id: request.task_id,
                    kind: *kind,
                    url: url.clone(),
                }];
                if context.selected_tab_id.is_none() {
                    facts.push(BrowserDurableFact::TabSelected {
                        tab_id,
                        context_id: request.context_id,
                        task_id: request.task_id,
                    });
                }
                Ok(facts)
            }
            BrowserAction::CloseTab => {
                let tab = self.require_tab(request)?;
                Ok(vec![BrowserDurableFact::TabClosed {
                    tab_id: tab_id_of(request)?,
                    context_id: tab.context_id,
                    task_id: request.task_id,
                }])
            }
            BrowserAction::SelectTab => {
                let tab = self.require_tab(request)?;
                Ok(vec![BrowserDurableFact::TabSelected {
                    tab_id: tab_id_of(request)?,
                    context_id: tab.context_id,
                    task_id: request.task_id,
                }])
            }
            BrowserAction::LinkArtifact { artifact_id } => {
                self.require_context(request)?;
                Ok(vec![BrowserDurableFact::ArtifactLinked {
                    context_id: request.context_id,
                    task_id: request.task_id,
                    artifact_id: *artifact_id,
                }])
            }
            _ => {
                if request.tab_id.is_some() {
                    self.require_tab(request)?;
                } else {
                    self.require_context(request)?;
                }
                Ok(Vec::new())
            }
        }
    }

    fn require_context(
        &self,
        request: &BrowserRequest,
    ) -> Result<&ContextState, BrowserContractError> {
        self.require_context_ids(request.task_id, request.context_id, request.generation)
    }

    fn require_context_ids(
        &self,
        task_id: TaskId,
        context_id: BrowserContextId,
        generation: u64,
    ) -> Result<&ContextState, BrowserContractError> {
        let Some(context) = self.contexts.get(&context_id) else {
            return Err(BrowserContractError::InvalidRequest);
        };
        if context.task_id != task_id {
            return Err(BrowserContractError::CrossTask);
        }
        if context.closed {
            return Err(BrowserContractError::InvalidRequest);
        }
        if context.generation != generation {
            return Err(BrowserContractError::GenerationMismatch);
        }
        Ok(context)
    }

    fn require_tab(&self, request: &BrowserRequest) -> Result<&TabState, BrowserContractError> {
        self.require_context(request)?;
        let tab_id = tab_id_of(request)?;
        let Some(tab) = self.tabs.get(&tab_id) else {
            return Err(BrowserContractError::InvalidRequest);
        };
        if tab.task_id != request.task_id {
            return Err(BrowserContractError::CrossTask);
        }
        if tab.context_id != request.context_id || tab.closed {
            return Err(BrowserContractError::InvalidRequest);
        }
        Ok(tab)
    }

    fn apply_fact(&mut self, fact: &BrowserDurableFact) -> Result<(), BrowserContractError> {
        match fact {
            BrowserDurableFact::RequestAccepted {
                request_id,
                task_id,
                context_id,
                tab_id,
                generation,
                action,
                privacy_class,
                permission,
                payload_hash,
                action_epoch,
                command_id,
            } => {
                let owner = self
                    .contexts
                    .get(context_id)
                    .map(|context| context.task_id)
                    .or_else(|| self.open_tasks.iter().find(|id| *id == task_id).copied());
                let Some(owner) = owner else {
                    return Err(BrowserContractError::ClosedTask);
                };
                if owner != *task_id {
                    return Err(BrowserContractError::CrossTask);
                }
                if let Some(context) = self.contexts.get(context_id) {
                    if context.task_id != *task_id {
                        return Err(BrowserContractError::CrossTask);
                    }
                    if context.generation != *generation {
                        return Err(BrowserContractError::GenerationMismatch);
                    }
                }
                self.receipts.entry(*request_id).or_insert(StoredReceipt {
                    request: BrowserRequest {
                        request_id: *request_id,
                        task_id: *task_id,
                        context_id: *context_id,
                        tab_id: *tab_id,
                        generation: *generation,
                        action: action.clone(),
                    },
                    accepted: BrowserAcceptedReceipt {
                        request_id: *request_id,
                        command_id: *command_id,
                        task_id: *task_id,
                        context_id: *context_id,
                        tab_id: *tab_id,
                        generation: *generation,
                        action: action.clone(),
                        facts: Vec::new(),
                        privacy_class: *privacy_class,
                        permission: *permission,
                        payload_hash: *payload_hash,
                        action_epoch: *action_epoch,
                    },
                    settled: !action.requires_host_settlement(),
                    settlement: None,
                });
            }
            BrowserDurableFact::PermissionDecided {
                context_id,
                task_id,
                permission,
                allowed,
            } => {
                let context = self
                    .contexts
                    .get_mut(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                if context.task_id != *task_id {
                    return Err(BrowserContractError::CrossTask);
                }
                context.permissions.insert(*permission, *allowed);
            }
            BrowserDurableFact::ArtifactLinked {
                context_id,
                task_id,
                artifact_id,
            } => {
                let context = self
                    .contexts
                    .get_mut(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                if context.task_id != *task_id {
                    return Err(BrowserContractError::CrossTask);
                }
                context.linked_artifacts.insert(*artifact_id);
            }
            BrowserDurableFact::RecipeIdentified {
                context_id,
                task_id,
                recipe_id,
            } => {
                let context = self
                    .contexts
                    .get_mut(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                if context.task_id != *task_id {
                    return Err(BrowserContractError::CrossTask);
                }
                context.recipe_id = Some(recipe_id.clone());
            }
            BrowserDurableFact::RecordingIdentified {
                context_id,
                task_id,
                recording_id,
            } => {
                let context = self
                    .contexts
                    .get_mut(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                if context.task_id != *task_id {
                    return Err(BrowserContractError::CrossTask);
                }
                context.recording_id = Some(recording_id.clone());
            }
            BrowserDurableFact::DownloadSettled {
                context_id,
                task_id,
                file_name,
                sha256_hex,
                artifact_id,
                ..
            } => {
                validate_identity(file_name)?;
                if let Some(digest) = sha256_hex {
                    if digest.len() != 64
                        || !digest
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                    {
                        return Err(BrowserContractError::InvalidRequest);
                    }
                }
                let context = self
                    .contexts
                    .get_mut(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                if context.task_id != *task_id {
                    return Err(BrowserContractError::CrossTask);
                }
                if let Some(artifact_id) = artifact_id {
                    context.linked_artifacts.insert(*artifact_id);
                }
            }
            BrowserDurableFact::ContextCreated {
                context_id,
                task_id,
                generation,
            } => {
                if !self.open_tasks.contains(task_id) {
                    return Err(BrowserContractError::ClosedTask);
                }
                if self.contexts.contains_key(context_id) {
                    return Err(BrowserContractError::InvalidRequest);
                }
                if self.contexts.len() >= MAX_BROWSER_CONTEXTS {
                    return Err(BrowserContractError::BoundExceeded);
                }
                self.contexts.insert(
                    *context_id,
                    ContextState {
                        task_id: *task_id,
                        generation: *generation,
                        selected_tab_id: None,
                        health: BrowserHealth::Healthy,
                        closed: false,
                        permissions: BTreeMap::new(),
                        linked_artifacts: BTreeSet::new(),
                        recipe_id: None,
                        recording_id: None,
                    },
                );
            }
            BrowserDurableFact::ContextClosed {
                context_id,
                task_id,
                ..
            } => {
                let context = self
                    .contexts
                    .get_mut(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                if context.task_id != *task_id {
                    return Err(BrowserContractError::CrossTask);
                }
                context.closed = true;
                for tab in self.tabs.values_mut() {
                    if tab.context_id == *context_id {
                        tab.closed = true;
                    }
                }
            }
            BrowserDurableFact::TabOpened {
                tab_id,
                context_id,
                task_id,
                kind,
                url,
            } => {
                validate_url(url)?;
                let context = self
                    .contexts
                    .get(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                if context.task_id != *task_id {
                    return Err(BrowserContractError::CrossTask);
                }
                if self.tabs.contains_key(tab_id) {
                    return Err(BrowserContractError::InvalidRequest);
                }
                self.tabs.insert(
                    *tab_id,
                    TabState {
                        context_id: *context_id,
                        task_id: *task_id,
                        kind: *kind,
                        committed_url: Some(url.clone()),
                        closed: false,
                    },
                );
            }
            BrowserDurableFact::TabClosed {
                tab_id, context_id, ..
            } => {
                let tab = self
                    .tabs
                    .get_mut(tab_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                tab.closed = true;
                if let Some(context) = self.contexts.get_mut(context_id) {
                    if context.selected_tab_id == Some(*tab_id) {
                        context.selected_tab_id = None;
                    }
                }
            }
            BrowserDurableFact::TabSelected {
                tab_id, context_id, ..
            } => {
                if !self.tabs.contains_key(tab_id) {
                    return Err(BrowserContractError::InvalidRequest);
                }
                let context = self
                    .contexts
                    .get_mut(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                context.selected_tab_id = Some(*tab_id);
            }
            BrowserDurableFact::NavigationCommitted { tab_id, url, .. } => {
                validate_url(url)?;
                let tab = self
                    .tabs
                    .get_mut(tab_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                tab.committed_url = Some(url.clone());
            }
            BrowserDurableFact::HealthTransitioned {
                context_id,
                to,
                generation,
                ..
            } => {
                let context = self
                    .contexts
                    .get_mut(context_id)
                    .ok_or(BrowserContractError::InvalidRequest)?;
                context.health = *to;
                context.generation = *generation;
            }
        }
        self.facts.push(fact.clone());
        Ok(())
    }
}

pub fn replay_browser_snapshot(
    facts: &[BrowserDurableFact],
) -> Result<BrowserIdentitySnapshot, BrowserContractError> {
    if facts.len() > MAX_BROWSER_JOURNAL_FACTS {
        return Err(BrowserContractError::BoundExceeded);
    }
    let mut book = BrowserBook::new();
    for fact in facts {
        book.open_task(fact.task_id())?;
    }
    book.apply_facts(facts)?;
    Ok(BrowserIdentitySnapshot {
        contexts: book
            .contexts
            .iter()
            .map(|(id, context)| context.view(*id))
            .collect(),
        tabs: book.tabs.iter().map(|(id, tab)| tab.view(*id)).collect(),
    })
}

fn tab_id_of(request: &BrowserRequest) -> Result<BrowserTabId, BrowserContractError> {
    request.tab_id.ok_or(BrowserContractError::InvalidRequest)
}

fn validate_url(raw: &str) -> Result<(), BrowserContractError> {
    if raw.len() > MAX_BROWSER_FACT_URL_BYTES {
        return Err(BrowserContractError::BoundExceeded);
    }
    let parsed = Url::parse(raw).map_err(|_| BrowserContractError::InvalidRequest)?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BrowserContractError::InvalidRequest);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(BrowserContractError::InvalidRequest);
    }
    if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() {
        return Ok(());
    }
    Err(BrowserContractError::InvalidRequest)
}

fn validate_identity(value: &str) -> Result<(), BrowserContractError> {
    if value.is_empty() {
        return Err(BrowserContractError::InvalidRequest);
    }
    if value.len() > MAX_BROWSER_IDENTITY_BYTES {
        return Err(BrowserContractError::BoundExceeded);
    }
    Ok(())
}

fn shareable_origin(raw: &str) -> Option<String> {
    let parsed = Url::parse(raw).ok()?;
    let host = parsed.host_str()?;
    let origin = match parsed.port() {
        Some(port) => format!("{}://{}:{}/", parsed.scheme(), host, port),
        None => format!("{}://{}/", parsed.scheme(), host),
    };
    Some(origin)
}

/// Wire/ClientModel URL: reject query/userinfo/fragment, then drop path secrets.
pub fn browser_wire_committed_url(raw: &str) -> Result<String, BrowserContractError> {
    validate_url(raw)?;
    shareable_origin(raw).ok_or(BrowserContractError::InvalidRequest)
}

fn key_bytes(key: BrowserPageKey) -> Vec<u8> {
    match key {
        BrowserPageKey::Context(id) => id.as_bytes().to_vec(),
        BrowserPageKey::Tab(id) => id.as_bytes().to_vec(),
    }
}

fn page_encoded_bytes(
    items: &[BrowserSnapshotRow],
    extra: &BrowserSnapshotRow,
) -> Result<u32, BrowserContractError> {
    let mut page = Vec::with_capacity(items.len() + 1);
    page.extend(items.iter().cloned());
    page.push(extra.clone());
    let bytes = rmp_serde::to_vec_named(&page).map_err(|_| BrowserContractError::BoundExceeded)?;
    u32::try_from(bytes.len()).map_err(|_| BrowserContractError::BoundExceeded)
}

fn request_payload_hash(request: &BrowserRequest) -> [u8; 32] {
    let bytes = rmp_serde::to_vec_named(request).unwrap_or_default();
    Sha256::digest(bytes).into()
}

fn fact_task_id(fact: &BrowserDurableFact) -> TaskId {
    match fact {
        BrowserDurableFact::RequestAccepted { task_id, .. }
        | BrowserDurableFact::ContextCreated { task_id, .. }
        | BrowserDurableFact::ContextClosed { task_id, .. }
        | BrowserDurableFact::TabOpened { task_id, .. }
        | BrowserDurableFact::TabClosed { task_id, .. }
        | BrowserDurableFact::TabSelected { task_id, .. }
        | BrowserDurableFact::NavigationCommitted { task_id, .. }
        | BrowserDurableFact::PermissionDecided { task_id, .. }
        | BrowserDurableFact::ArtifactLinked { task_id, .. }
        | BrowserDurableFact::RecipeIdentified { task_id, .. }
        | BrowserDurableFact::RecordingIdentified { task_id, .. }
        | BrowserDurableFact::DownloadSettled { task_id, .. }
        | BrowserDurableFact::HealthTransitioned { task_id, .. } => *task_id,
    }
}

pub fn perform_browser_host_effect(_request: &BrowserRequest) -> Result<(), BrowserContractError> {
    Err(BrowserContractError::HostEffectUnavailable)
}

pub fn decode_browser_request_json(raw: &str) -> Result<BrowserRequest, BrowserContractError> {
    if raw.len() > BROWSER_WIRE_MAX_BYTES {
        return Err(BrowserContractError::BoundExceeded);
    }
    if wire_exceeds_phase1_caps(raw) {
        return Err(BrowserContractError::BoundExceeded);
    }
    serde_json::from_str(raw).map_err(|_| BrowserContractError::InvalidRequest)
}

pub fn decode_browser_request_wire(bytes: &[u8]) -> Result<BrowserRequest, BrowserContractError> {
    if bytes.len() > BROWSER_WIRE_MAX_BYTES {
        return Err(BrowserContractError::BoundExceeded);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        if wire_exceeds_phase1_caps(text) {
            return Err(BrowserContractError::BoundExceeded);
        }
        return decode_browser_request_json(text);
    }
    Err(BrowserContractError::InvalidRequest)
}

fn wire_exceeds_phase1_caps(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    json_exceeds_caps(&value, 0)
}

fn json_exceeds_caps(value: &serde_json::Value, depth: u16) -> bool {
    if depth > BROWSER_WIRE_MAX_DEPTH {
        return true;
    }
    match value {
        serde_json::Value::Array(items) => {
            items.len() as u32 > BROWSER_WIRE_MAX_ITEMS
                || items.iter().any(|item| json_exceeds_caps(item, depth + 1))
        }
        serde_json::Value::Object(map) => {
            map.len() as u32 > BROWSER_WIRE_MAX_ITEMS
                || map.values().any(|item| json_exceeds_caps(item, depth + 1))
        }
        serde_json::Value::String(text) => text.len() > MAX_BROWSER_FACT_URL_BYTES,
        _ => false,
    }
}

impl BrowserDurableFact {
    pub fn task_id(&self) -> TaskId {
        fact_task_id(self)
    }
}

#[cfg(test)]
mod host_surface_settler_tests {
    use super::*;
    use crate::domain::id::ResourceId;

    #[test]
    fn epoch_only_bind_cannot_match_a_host_surface() {
        let intent = BrowserHostSettleIntent::bind(
            CommandId::new(),
            OperationId::new(),
            BrowserRequestId::new(),
            TaskId::new(),
            BrowserContextId::new(),
            1,
            1,
        )
        .expect("intent");
        assert_eq!(
            intent.matches_host_surface(
                intent.task_id(),
                intent.context_id(),
                ResourceId::new(),
                1
            ),
            Err(BrowserContractError::InvalidRequest)
        );
    }

    #[test]
    fn host_owned_token_settles_matching_live_identity() {
        let task_id = TaskId::new();
        let context_id = BrowserContextId::new();
        let resource_id = ResourceId::new();
        let request_id = BrowserRequestId::new();
        let intent = BrowserHostSettleIntent::bind_host_surface(
            CommandId::new(),
            OperationId::new(),
            request_id,
            task_id,
            context_id,
            resource_id,
            3,
            2,
        )
        .expect("surface intent");
        let authority = BrowserServiceAuthority::issue(
            &BrowserServiceIssuer::for_host_service(),
            task_id,
            context_id,
            resource_id,
            3,
        )
        .expect("authority");
        let token = BrowserServiceSettlerToken::from_host_owned_surface(&authority, &intent)
            .expect("token");
        let outcome = token.settle_accepted_hold(&intent).expect("settled");
        assert_eq!(outcome.task_id, task_id);
        assert_eq!(outcome.context_id, context_id);
        assert_eq!(outcome.request_id, request_id);
        assert_eq!(
            outcome.settlement,
            BrowserSettlement::Recovered { generation: 3 }
        );
    }

    #[test]
    fn host_owned_token_rejects_cross_task_or_resource() {
        let intent = BrowserHostSettleIntent::bind_host_surface(
            CommandId::new(),
            OperationId::new(),
            BrowserRequestId::new(),
            TaskId::new(),
            BrowserContextId::new(),
            ResourceId::new(),
            1,
            1,
        )
        .expect("intent");
        let foreign = BrowserServiceAuthority::issue(
            &BrowserServiceIssuer::for_host_service(),
            TaskId::new(),
            intent.context_id(),
            intent.resource_id().expect("resource"),
            1,
        )
        .expect("foreign authority");
        assert_eq!(
            BrowserServiceSettlerToken::from_host_owned_surface(&foreign, &intent)
                .err()
                .expect("cross-task authority must not mint a token"),
            BrowserIntegrationHold::WebViewSurfaceAbsent
        );
    }
}
