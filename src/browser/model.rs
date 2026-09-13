use super::redact_browser_text;
use rmcp::schemars;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_BROWSER_INTERACTION_EPOCH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrowserInteractionEpochError;

pub(crate) fn next_browser_interaction_epoch() -> Result<u64, BrowserInteractionEpochError> {
    NEXT_BROWSER_INTERACTION_EPOCH
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| BrowserInteractionEpochError)
}

#[cfg(test)]
mod interaction_epoch_tests {
    use super::*;

    #[test]
    fn interaction_epoch_exhaustion_returns_typed_failure_without_panicking() {
        let previous = NEXT_BROWSER_INTERACTION_EPOCH.swap(u64::MAX, Ordering::SeqCst);
        let outcome = std::panic::catch_unwind(next_browser_interaction_epoch);
        NEXT_BROWSER_INTERACTION_EPOCH.store(previous, Ordering::SeqCst);

        assert!(
            outcome.is_ok(),
            "interaction epoch exhaustion must fail closed without panicking"
        );
        assert!(outcome.unwrap().is_err());
    }
}

#[cfg(test)]
mod browser_error_wire_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn public_browser_error_serde_redacts_paths_urls_and_unbounded_diagnostics() {
        const SENTINEL: &str = "browser-error-wire-secret-sentinel";
        let errors = [
            BrowserError::MissingFile {
                path: PathBuf::from(format!(r"C:\Users\secret\{SENTINEL}.txt")),
            },
            BrowserError::NavigationFailure {
                url: format!("https://{SENTINEL}.invalid/private"),
                message: format!("backend diagnostic {SENTINEL}"),
            },
            BrowserError::CrashedView {
                message: format!("{SENTINEL}{}", "x".repeat(4_096)),
            },
            BrowserError::Io {
                operation: SENTINEL.to_string(),
                path: PathBuf::from(format!(r"C:\absolute\{SENTINEL}")),
                message: SENTINEL.to_string(),
            },
        ];

        for error in errors {
            let encoded = serde_json::to_string(&error).expect("serialize public browser error");
            assert!(!encoded.contains(SENTINEL), "{encoded}");
            assert!(!encoded.contains("https://"), "{encoded}");
            assert!(!encoded.contains("C:\\"), "{encoded}");
            assert!(encoded.len() <= 512, "public errors must stay bounded");

            let decoded: BrowserError =
                serde_json::from_str(&encoded).expect("deserialize public browser error");
            assert_eq!(decoded.public_message(), error.public_message());
        }
    }
}

pub(super) fn browser_annotation_urls_equivalent(left: &str, right: &str) -> bool {
    redact_browser_text(left) == redact_browser_text(right)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct BrowserWorkspaceKey {
    pub project_id: String,
    pub ai_tab_id: String,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    rmcp::schemars::JsonSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
)]
#[serde(transparent)]
pub struct BrowserRevision(pub u64);

/// Monotonic revision for the next-prompt annotation queue.
///
/// This is intentionally independent from [`BrowserRevision`], which tracks
/// live page/DOM state and invalidates semantic element references.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Default,
)]
#[serde(transparent)]
pub struct BrowserAttachmentRevision(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(transparent)]
pub struct BrowserResourceId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    pub scale_percent: u16,
}

impl Default for BrowserViewport {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            scale_percent: 100,
        }
    }
}

#[derive(
    Debug, Clone, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq, Default,
)]
#[serde(default, rename_all = "camelCase")]
pub struct BrowserLocator {
    pub accessibility_role: Option<String>,
    pub accessibility_name: Option<String>,
    pub test_id: Option<String>,
    pub css_selectors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserElementRef {
    pub revision: BrowserRevision,
    pub locator: BrowserLocator,
    pub backend_node_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserTabSnapshot {
    pub id: String,
    pub title: String,
    pub url: String,
    pub viewport: BrowserViewport,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserBounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "camelCase")]
pub enum BrowserAnnotationKind {
    #[default]
    Element,
    Region,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAnnotation {
    pub id: String,
    #[serde(default)]
    pub kind: BrowserAnnotationKind,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub anchor_revision: BrowserRevision,
    pub comment: String,
    pub url: String,
    pub locator: BrowserLocator,
    pub bounds: BrowserBounds,
    pub viewport: BrowserViewport,
    pub screenshot_resource: BrowserResourceId,
    pub computed_styles: BTreeMap<String, String>,
    pub resolved: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserJournalActor {
    User,
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserJournalEntry {
    pub id: String,
    pub actor: BrowserJournalActor,
    pub intent: String,
    pub url: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub result: String,
    pub resource_ids: Vec<BrowserResourceId>,
}

impl BrowserWorkspaceKey {
    pub fn new(
        project_id: impl Into<String>,
        ai_tab_id: impl Into<String>,
    ) -> Result<Self, BrowserError> {
        let project_id = project_id.into();
        if project_id.trim().is_empty() {
            return Err(BrowserError::InvalidWorkspaceKey {
                field: "projectId".to_string(),
            });
        }

        let ai_tab_id = ai_tab_id.into();
        if ai_tab_id.trim().is_empty() {
            return Err(BrowserError::InvalidWorkspaceKey {
                field: "aiTabId".to_string(),
            });
        }

        Ok(Self {
            project_id,
            ai_tab_id,
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum BrowserError {
    InvalidWorkspaceKey {
        field: String,
    },
    InvalidInvocation {
        field: String,
    },
    InvalidAnnotation {
        field: String,
        message: String,
    },
    MissingAnnotation {
        id: String,
    },
    StaleReference {
        expected: BrowserRevision,
        actual: BrowserRevision,
    },
    MissingFile {
        path: PathBuf,
    },
    MissingResource {
        id: BrowserResourceId,
    },
    ResourceTooLarge {
        byte_size: u64,
        limit: u64,
    },
    ResourceRootBusy,
    ResourceRootUnavailable,
    OutsideWorkspace {
        path: PathBuf,
    },
    InvalidRecipe {
        message: String,
    },
    UnsupportedRecipeVersion {
        version: u32,
    },
    RecordingResourceUnavailable,
    Interrupted,
    InteractionEpochExhausted,
    CancellationEpochExhausted,
    Timeout {
        operation: String,
    },
    NavigationFailure {
        url: String,
        message: String,
    },
    InitializingView {
        tab_id: String,
    },
    CrashedView {
        message: String,
    },
    LocatorNotFound {
        target: BrowserLocatorFailureTarget,
    },
    BlockedPermission {
        permission: String,
    },
    UnavailablePlatform {
        platform: String,
    },
    Io {
        operation: String,
        path: PathBuf,
        message: String,
    },
}

const PUBLIC_BROWSER_REDACTED: &str = "<redacted>";

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum BrowserErrorPublicWire {
    InvalidWorkspaceKey {
        field: String,
    },
    InvalidInvocation {
        field: String,
    },
    InvalidAnnotation {
        field: String,
        message: String,
    },
    MissingAnnotation {
        id: String,
    },
    StaleReference {
        expected: BrowserRevision,
        actual: BrowserRevision,
    },
    MissingFile {
        path: String,
    },
    MissingResource {
        id: String,
    },
    ResourceTooLarge {
        byte_size: u64,
        limit: u64,
    },
    ResourceRootBusy,
    ResourceRootUnavailable,
    OutsideWorkspace {
        path: String,
    },
    InvalidRecipe {
        message: String,
    },
    UnsupportedRecipeVersion {
        version: u32,
    },
    RecordingResourceUnavailable,
    Interrupted,
    InteractionEpochExhausted,
    CancellationEpochExhausted,
    Timeout {
        operation: String,
    },
    NavigationFailure {
        url: String,
        message: String,
    },
    InitializingView {
        tab_id: String,
    },
    CrashedView {
        message: String,
    },
    LocatorNotFound {
        target: BrowserLocatorFailureTarget,
    },
    BlockedPermission {
        permission: String,
    },
    UnavailablePlatform {
        platform: String,
    },
    Io {
        operation: String,
        path: String,
        message: String,
    },
}

impl BrowserError {
    fn public_wire(&self) -> BrowserErrorPublicWire {
        match self {
            Self::InvalidWorkspaceKey { field } => BrowserErrorPublicWire::InvalidWorkspaceKey {
                field: public_browser_field(field),
            },
            Self::InvalidInvocation { field } => BrowserErrorPublicWire::InvalidInvocation {
                field: public_browser_field(field),
            },
            Self::InvalidAnnotation { field, .. } => BrowserErrorPublicWire::InvalidAnnotation {
                field: public_browser_field(field),
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::MissingAnnotation { .. } => BrowserErrorPublicWire::MissingAnnotation {
                id: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::StaleReference { expected, actual } => BrowserErrorPublicWire::StaleReference {
                expected: *expected,
                actual: *actual,
            },
            Self::MissingFile { .. } => BrowserErrorPublicWire::MissingFile {
                path: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::MissingResource { .. } => BrowserErrorPublicWire::MissingResource {
                id: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::ResourceTooLarge { byte_size, limit } => {
                BrowserErrorPublicWire::ResourceTooLarge {
                    byte_size: *byte_size,
                    limit: *limit,
                }
            }
            Self::ResourceRootBusy => BrowserErrorPublicWire::ResourceRootBusy,
            Self::ResourceRootUnavailable => BrowserErrorPublicWire::ResourceRootUnavailable,
            Self::OutsideWorkspace { .. } => BrowserErrorPublicWire::OutsideWorkspace {
                path: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::InvalidRecipe { .. } => BrowserErrorPublicWire::InvalidRecipe {
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::UnsupportedRecipeVersion { version } => {
                BrowserErrorPublicWire::UnsupportedRecipeVersion { version: *version }
            }
            Self::RecordingResourceUnavailable => {
                BrowserErrorPublicWire::RecordingResourceUnavailable
            }
            Self::Interrupted => BrowserErrorPublicWire::Interrupted,
            Self::InteractionEpochExhausted => BrowserErrorPublicWire::InteractionEpochExhausted,
            Self::CancellationEpochExhausted => BrowserErrorPublicWire::CancellationEpochExhausted,
            Self::Timeout { operation } => BrowserErrorPublicWire::Timeout {
                operation: public_browser_operation(operation),
            },
            Self::NavigationFailure { .. } => BrowserErrorPublicWire::NavigationFailure {
                url: PUBLIC_BROWSER_REDACTED.to_string(),
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::InitializingView { .. } => BrowserErrorPublicWire::InitializingView {
                tab_id: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::CrashedView { .. } => BrowserErrorPublicWire::CrashedView {
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::LocatorNotFound { target } => {
                BrowserErrorPublicWire::LocatorNotFound { target: *target }
            }
            Self::BlockedPermission { .. } => BrowserErrorPublicWire::BlockedPermission {
                permission: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            Self::UnavailablePlatform { platform } => BrowserErrorPublicWire::UnavailablePlatform {
                platform: public_browser_platform(platform),
            },
            Self::Io { operation, .. } => BrowserErrorPublicWire::Io {
                operation: public_browser_operation(operation),
                path: PUBLIC_BROWSER_REDACTED.to_string(),
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
        }
    }
}

impl From<BrowserErrorPublicWire> for BrowserError {
    fn from(wire: BrowserErrorPublicWire) -> Self {
        match wire {
            BrowserErrorPublicWire::InvalidWorkspaceKey { field } => Self::InvalidWorkspaceKey {
                field: public_browser_field(&field),
            },
            BrowserErrorPublicWire::InvalidInvocation { field } => Self::InvalidInvocation {
                field: public_browser_field(&field),
            },
            BrowserErrorPublicWire::InvalidAnnotation { field, .. } => Self::InvalidAnnotation {
                field: public_browser_field(&field),
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            BrowserErrorPublicWire::MissingAnnotation { .. } => Self::MissingAnnotation {
                id: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            BrowserErrorPublicWire::StaleReference { expected, actual } => {
                Self::StaleReference { expected, actual }
            }
            BrowserErrorPublicWire::MissingFile { .. } => Self::MissingFile {
                path: PathBuf::from(PUBLIC_BROWSER_REDACTED),
            },
            BrowserErrorPublicWire::MissingResource { .. } => Self::MissingResource {
                id: BrowserResourceId(PUBLIC_BROWSER_REDACTED.to_string()),
            },
            BrowserErrorPublicWire::ResourceTooLarge { byte_size, limit } => {
                Self::ResourceTooLarge { byte_size, limit }
            }
            BrowserErrorPublicWire::ResourceRootBusy => Self::ResourceRootBusy,
            BrowserErrorPublicWire::ResourceRootUnavailable => Self::ResourceRootUnavailable,
            BrowserErrorPublicWire::OutsideWorkspace { .. } => Self::OutsideWorkspace {
                path: PathBuf::from(PUBLIC_BROWSER_REDACTED),
            },
            BrowserErrorPublicWire::InvalidRecipe { .. } => Self::InvalidRecipe {
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            BrowserErrorPublicWire::UnsupportedRecipeVersion { version } => {
                Self::UnsupportedRecipeVersion { version }
            }
            BrowserErrorPublicWire::RecordingResourceUnavailable => {
                Self::RecordingResourceUnavailable
            }
            BrowserErrorPublicWire::Interrupted => Self::Interrupted,
            BrowserErrorPublicWire::InteractionEpochExhausted => Self::InteractionEpochExhausted,
            BrowserErrorPublicWire::CancellationEpochExhausted => Self::CancellationEpochExhausted,
            BrowserErrorPublicWire::Timeout { operation } => Self::Timeout {
                operation: public_browser_operation(&operation),
            },
            BrowserErrorPublicWire::NavigationFailure { .. } => Self::NavigationFailure {
                url: PUBLIC_BROWSER_REDACTED.to_string(),
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            BrowserErrorPublicWire::InitializingView { .. } => Self::InitializingView {
                tab_id: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            BrowserErrorPublicWire::CrashedView { .. } => Self::CrashedView {
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            BrowserErrorPublicWire::LocatorNotFound { target } => Self::LocatorNotFound { target },
            BrowserErrorPublicWire::BlockedPermission { .. } => Self::BlockedPermission {
                permission: PUBLIC_BROWSER_REDACTED.to_string(),
            },
            BrowserErrorPublicWire::UnavailablePlatform { platform } => Self::UnavailablePlatform {
                platform: public_browser_platform(&platform),
            },
            BrowserErrorPublicWire::Io { operation, .. } => Self::Io {
                operation: public_browser_operation(&operation),
                path: PathBuf::from(PUBLIC_BROWSER_REDACTED),
                message: PUBLIC_BROWSER_REDACTED.to_string(),
            },
        }
    }
}

impl Serialize for BrowserError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.public_wire().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BrowserError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        BrowserErrorPublicWire::deserialize(deserializer).map(Into::into)
    }
}

fn public_browser_field(value: &str) -> String {
    const KNOWN_FIELDS: &[&str] = &[
        "projectId",
        "aiTabId",
        "intent",
        "operationId",
        "tabId",
        "annotationId",
        "comment",
        "url",
        "locator",
        "kind",
        "computedStyles",
        "bounds",
        "ipcBody",
        "draftId",
        "secretSidecar",
        "repairSidecar",
        "repairPreviewSidecar",
        "repairApplySidecar",
        "resume",
        "confirm",
    ];
    if KNOWN_FIELDS.contains(&value) {
        value.to_string()
    } else {
        PUBLIC_BROWSER_REDACTED.to_string()
    }
}

fn public_browser_operation(value: &str) -> String {
    const KNOWN_OPERATIONS: &[&str] = &[
        "snapshot",
        "screenshot",
        "navigate",
        "reload",
        "status",
        "workspaceState",
        "ensure",
        "setPaneOpen",
        "setAnnotationMode",
        "captureAnnotation",
        "saveAnnotationDraft",
        "cancelAnnotationDraft",
        "annotations",
        "recording",
        "listTabs",
        "createTab",
        "selectTab",
        "closeTab",
        "back",
        "forward",
        "updateViewport",
        "openDevTools",
        "stop",
        "resetWorkspace",
        "clearProjectProfile",
        "secretType",
        "wait",
        "act",
        "console",
        "network",
        "performance",
        "upload",
        "downloads",
        "repairHighlight",
        "repairClearHighlight",
        "repairValidate",
        "cdp",
        "downloadDirectory",
    ];
    if KNOWN_OPERATIONS.contains(&value) {
        value.to_string()
    } else {
        PUBLIC_BROWSER_REDACTED.to_string()
    }
}

fn public_browser_platform(value: &str) -> String {
    const KNOWN_PLATFORMS: &[&str] = &[
        "windows", "macos", "linux", "android", "ios", "freebsd", "openbsd", "netbsd",
    ];
    if KNOWN_PLATFORMS.contains(&value) {
        value.to_string()
    } else {
        "unknown".to_string()
    }
}

impl From<BrowserInteractionEpochError> for BrowserError {
    fn from(_: BrowserInteractionEpochError) -> Self {
        Self::InteractionEpochExhausted
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserLocatorFailureTarget {
    Primary,
    Source,
    Destination,
}

impl BrowserError {
    pub(crate) fn public_message(&self) -> &'static str {
        match self {
            Self::InvalidWorkspaceKey { .. } => "browser workspace key is invalid",
            Self::InvalidInvocation { .. } => "browser invocation is invalid",
            Self::InvalidAnnotation { .. } => "browser annotation is invalid",
            Self::MissingAnnotation { .. } => "browser annotation was not found",
            Self::StaleReference { .. } => "browser element reference is stale",
            Self::MissingFile { .. } => "browser file was not found",
            Self::MissingResource { .. } => "browser resource was not found",
            Self::ResourceTooLarge { .. } => "browser resource exceeds its size limit",
            Self::ResourceRootBusy => "browser resource root is busy",
            Self::ResourceRootUnavailable => "browser resource root is unavailable",
            Self::OutsideWorkspace { .. } => "browser file is outside the project workspace",
            Self::InvalidRecipe { .. } => "browser recipe is invalid",
            Self::UnsupportedRecipeVersion { .. } => "browser recipe schema version is unsupported",
            Self::RecordingResourceUnavailable => {
                "browser recording review resource is unavailable"
            }
            Self::Interrupted => "browser operation was interrupted",
            Self::InteractionEpochExhausted => "browser interaction authority is exhausted",
            Self::CancellationEpochExhausted => "browser cancellation authority is exhausted",
            Self::Timeout { .. } => "browser operation timed out",
            Self::NavigationFailure { .. } => "browser navigation failed",
            Self::InitializingView { .. } => "browser view is still initializing",
            Self::CrashedView { .. } => "browser view crashed",
            Self::LocatorNotFound { .. } => "browser locator target was not found",
            Self::BlockedPermission { .. } => "browser permission was blocked",
            Self::UnavailablePlatform { .. } => "browser platform is unavailable",
            Self::Io { .. } => "browser host storage operation failed",
        }
    }
}

impl fmt::Debug for BrowserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("BrowserError")
            .field(&self.public_message())
            .finish()
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.public_message())
    }
}

impl std::error::Error for BrowserError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct BrowserWorkspaceSnapshot {
    pub pane_open: bool,
    pub split_percent: u8,
    pub revision: BrowserRevision,
    pub tabs: Vec<BrowserTabSnapshot>,
    pub selected_tab_id: Option<String>,
    pub annotations: Vec<BrowserAnnotation>,
    pub pending_annotation_revision: BrowserAttachmentRevision,
    pub pending_annotation_ids: Vec<String>,
    pub journal_entries: Vec<BrowserJournalEntry>,
}

impl Default for BrowserWorkspaceSnapshot {
    fn default() -> Self {
        Self {
            pane_open: false,
            split_percent: 50,
            revision: BrowserRevision::default(),
            tabs: Vec::new(),
            selected_tab_id: None,
            annotations: Vec::new(),
            pending_annotation_revision: BrowserAttachmentRevision::default(),
            pending_annotation_ids: Vec::new(),
            journal_entries: Vec::new(),
        }
    }
}

impl BrowserWorkspaceSnapshot {
    pub fn set_split_percent(&mut self, split_percent: u8) {
        self.split_percent = split_percent.clamp(25, 75);
    }

    pub fn advance_revision(&mut self) -> BrowserRevision {
        self.revision.0 = self.revision.0.saturating_add(1);
        self.revision
    }

    fn advance_pending_annotation_revision(&mut self) -> BrowserAttachmentRevision {
        self.pending_annotation_revision.0 = self.pending_annotation_revision.0.saturating_add(1);
        self.pending_annotation_revision
    }

    pub fn validate_element_ref(&self, element: &BrowserElementRef) -> Result<(), BrowserError> {
        if element.revision != self.revision {
            return Err(BrowserError::StaleReference {
                expected: self.revision,
                actual: element.revision,
            });
        }

        Ok(())
    }

    pub fn save_annotation(
        &mut self,
        mut annotation: BrowserAnnotation,
    ) -> Result<(), BrowserError> {
        annotation.id = annotation.id.trim().to_string();
        if annotation.id.is_empty() {
            return Err(BrowserError::InvalidAnnotation {
                field: "id".to_string(),
                message: "cannot be blank".to_string(),
            });
        }
        annotation.comment = annotation.comment.trim().to_string();
        if annotation.comment.is_empty() {
            return Err(BrowserError::InvalidAnnotation {
                field: "comment".to_string(),
                message: "cannot be blank".to_string(),
            });
        }
        if self
            .annotations
            .iter()
            .any(|existing| existing.id == annotation.id)
        {
            return Err(BrowserError::InvalidAnnotation {
                field: "id".to_string(),
                message: format!("{} already exists", annotation.id),
            });
        }

        let id = annotation.id.clone();
        self.annotations.push(annotation);
        if !self
            .pending_annotation_ids
            .iter()
            .any(|pending| pending == &id)
        {
            self.pending_annotation_ids.push(id);
            self.advance_pending_annotation_revision();
        }
        Ok(())
    }

    pub fn annotation(&self, id: &str) -> Result<&BrowserAnnotation, BrowserError> {
        self.annotations
            .iter()
            .find(|annotation| annotation.id == id)
            .ok_or_else(|| BrowserError::MissingAnnotation { id: id.to_string() })
    }

    pub fn set_annotation_resolved(
        &mut self,
        id: &str,
        resolved: bool,
    ) -> Result<bool, BrowserError> {
        let annotation = self
            .annotations
            .iter_mut()
            .find(|annotation| annotation.id == id)
            .ok_or_else(|| BrowserError::MissingAnnotation { id: id.to_string() })?;
        let changed = annotation.resolved != resolved;
        annotation.resolved = resolved;
        Ok(changed)
    }

    pub fn delete_annotation(&mut self, id: &str) -> Result<BrowserAnnotation, BrowserError> {
        let index = self
            .annotations
            .iter()
            .position(|annotation| annotation.id == id)
            .ok_or_else(|| BrowserError::MissingAnnotation { id: id.to_string() })?;
        self.remove_pending_annotation(id);
        Ok(self.annotations.remove(index))
    }

    pub fn remove_pending_annotation(&mut self, id: &str) -> bool {
        let previous_len = self.pending_annotation_ids.len();
        self.pending_annotation_ids.retain(|pending| pending != id);
        let changed = previous_len != self.pending_annotation_ids.len();
        if changed {
            self.advance_pending_annotation_revision();
        }
        changed
    }

    pub fn acknowledge_pending_annotations(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let previous_len = self.pending_annotation_ids.len();
        self.pending_annotation_ids
            .retain(|pending| !ids.iter().any(|acknowledged| acknowledged == pending));
        if previous_len != self.pending_annotation_ids.len() {
            self.advance_pending_annotation_revision();
        }
    }

    pub fn annotation_anchor_is_stale(&self, id: &str) -> Result<bool, BrowserError> {
        let annotation = self.annotation(id)?;
        if annotation.tab_id.is_empty() || annotation.anchor_revision != self.revision {
            return Ok(true);
        }
        Ok(self
            .tabs
            .iter()
            .find(|tab| tab.id == annotation.tab_id)
            .is_none_or(|tab| !browser_annotation_urls_equivalent(&tab.url, &annotation.url)))
    }

    pub fn pinned_annotation_resource_ids(&self) -> BTreeSet<BrowserResourceId> {
        let mut pinned = BTreeSet::new();
        for annotation in &self.annotations {
            if !annotation.resolved
                || self
                    .pending_annotation_ids
                    .iter()
                    .any(|pending| pending == &annotation.id)
            {
                pinned.insert(annotation.screenshot_resource.clone());
            }
        }
        for entry in &self.journal_entries {
            pinned.extend(entry.resource_ids.iter().cloned());
        }
        pinned
    }
}
