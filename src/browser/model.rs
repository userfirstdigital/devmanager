use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAnnotation {
    pub id: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum BrowserError {
    InvalidWorkspaceKey {
        field: String,
    },
    InvalidInvocation {
        field: String,
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
    OutsideWorkspace {
        path: PathBuf,
    },
    InvalidRecipe {
        message: String,
    },
    UnsupportedRecipeVersion {
        version: u32,
    },
    Interrupted,
    Timeout {
        operation: String,
    },
    NavigationFailure {
        url: String,
        message: String,
    },
    CrashedView {
        message: String,
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

impl fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWorkspaceKey { field } => {
                write!(
                    formatter,
                    "browser workspace key field {field} cannot be blank"
                )
            }
            Self::InvalidInvocation { field } => {
                write!(
                    formatter,
                    "browser invocation field {field} cannot be blank"
                )
            }
            Self::StaleReference { expected, actual } => write!(
                formatter,
                "stale browser element reference: expected revision {}, got {}",
                expected.0, actual.0
            ),
            Self::MissingFile { path } => {
                write!(formatter, "browser file does not exist: {}", path.display())
            }
            Self::MissingResource { id } => {
                write!(formatter, "browser resource does not exist: {}", id.0)
            }
            Self::ResourceTooLarge { byte_size, limit } => write!(
                formatter,
                "browser resource size {byte_size} exceeds limit {limit}"
            ),
            Self::OutsideWorkspace { path } => write!(
                formatter,
                "browser file is outside the project workspace: {}",
                path.display()
            ),
            Self::InvalidRecipe { message } => {
                write!(formatter, "invalid browser recipe: {message}")
            }
            Self::UnsupportedRecipeVersion { version } => {
                write!(
                    formatter,
                    "unsupported browser recipe schema version {version}"
                )
            }
            Self::Interrupted => formatter.write_str("browser operation was interrupted"),
            Self::Timeout { operation } => {
                write!(formatter, "browser operation timed out: {operation}")
            }
            Self::NavigationFailure { url, message } => {
                write!(formatter, "browser navigation failed for {url}: {message}")
            }
            Self::CrashedView { message } => {
                write!(formatter, "browser view crashed: {message}")
            }
            Self::BlockedPermission { permission } => {
                write!(formatter, "browser permission was blocked: {permission}")
            }
            Self::UnavailablePlatform { platform } => {
                write!(formatter, "browser is unavailable on platform {platform}")
            }
            Self::Io {
                operation,
                path,
                message,
            } => write!(
                formatter,
                "browser {operation} failed for {}: {message}",
                path.display()
            ),
        }
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

    pub fn validate_element_ref(&self, element: &BrowserElementRef) -> Result<(), BrowserError> {
        if element.revision != self.revision {
            return Err(BrowserError::StaleReference {
                expected: self.revision,
                actual: element.revision,
            });
        }

        Ok(())
    }
}
