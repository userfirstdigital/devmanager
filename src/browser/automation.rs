use super::{
    BrowserBounds, BrowserElementRef, BrowserLocator, BrowserRevision, BrowserRisk,
    BrowserWorkspaceSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;

pub const MAX_BROWSER_ACTIONS: usize = 32;
pub const MAX_BROWSER_JOURNAL_ENTRIES: usize = 100;
pub const REDACTED_VALUE: &str = "[redacted]";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct BrowserActionTarget {
    pub element_ref: Option<BrowserElementRef>,
    pub locator: BrowserLocator,
    pub coordinates: Option<BrowserPoint>,
}

impl BrowserActionTarget {
    pub fn from_element_ref(element_ref: BrowserElementRef) -> Self {
        Self {
            locator: element_ref.locator.clone(),
            element_ref: Some(element_ref),
            coordinates: None,
        }
    }

    pub fn resolution_order(&self) -> Vec<BrowserLocatorStrategy> {
        let mut strategies = Vec::new();
        if let Some(test_id) = self
            .locator
            .test_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            strategies.push(BrowserLocatorStrategy::TestId(test_id.to_string()));
        }
        if let (Some(role), Some(name)) = (
            self.locator.accessibility_role.as_deref(),
            self.locator.accessibility_name.as_deref(),
        ) {
            if !role.is_empty() && !name.is_empty() {
                strategies.push(BrowserLocatorStrategy::Accessibility {
                    role: role.to_string(),
                    name: name.to_string(),
                });
            }
        }
        strategies.extend(
            self.locator
                .css_selectors
                .iter()
                .filter(|selector| !selector.trim().is_empty())
                .cloned()
                .map(BrowserLocatorStrategy::Css),
        );
        if let Some(point) = self.coordinates {
            strategies.push(BrowserLocatorStrategy::Coordinates(point));
        }
        strategies
    }

    fn diagnostic_name(&self) -> String {
        self.locator
            .test_id
            .as_deref()
            .or(self.locator.accessibility_name.as_deref())
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "element".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserLocatorStrategy {
    TestId(String),
    Accessibility { role: String, name: String },
    Css(String),
    Coordinates(BrowserPoint),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "operation",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserAction {
    Click {
        target: BrowserActionTarget,
    },
    Hover {
        target: BrowserActionTarget,
    },
    Focus {
        target: BrowserActionTarget,
    },
    Type {
        target: BrowserActionTarget,
        text: String,
    },
    Clear {
        target: BrowserActionTarget,
    },
    Select {
        target: BrowserActionTarget,
        values: Vec<String>,
    },
    Keypress {
        target: Option<BrowserActionTarget>,
        key: String,
    },
    Scroll {
        target: Option<BrowserActionTarget>,
        delta_x: i32,
        delta_y: i32,
    },
    DragDrop {
        source: BrowserActionTarget,
        destination: BrowserActionTarget,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserScreenshotMode {
    Viewport,
    FullPage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserWaitCondition {
    Duration { duration_ms: u64 },
    Url { value: String, exact: bool },
    Load,
    ElementPresent { target: BrowserActionTarget },
    ElementVisible { target: BrowserActionTarget },
    ElementHidden { target: BrowserActionTarget },
    TextPresent { text: String },
    TextAbsent { text: String },
    JavaScript { predicate: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserConsoleOperation {
    List,
    Clear,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserNetworkOperation {
    List,
    Clear,
    Body,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserPerformanceOperation {
    Snapshot,
    TraceStart,
    TraceStop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserDownloadOperation {
    List,
    Reveal,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshotSummary {
    pub tab_id: String,
    pub url: String,
    pub revision: BrowserRevision,
    pub element_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserWaitResult {
    pub matched: bool,
    pub elapsed_ms: u64,
    pub revision: BrowserRevision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserActionResult {
    pub completed_actions: usize,
    pub revision: BrowserRevision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConsoleEntry {
    pub sequence: u64,
    pub level: String,
    pub message: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserNetworkEntry {
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub status: Option<u16>,
    pub failed: bool,
    pub body_available: bool,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPerformanceSnapshot {
    pub navigation: Value,
    pub entries: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserUploadResult {
    pub files: Vec<PathBuf>,
    pub revision: BrowserRevision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDownloadEntry {
    pub id: String,
    pub file_name: String,
    pub byte_size: u64,
    pub completed: bool,
}

impl BrowserAction {
    pub fn target(&self) -> Option<&BrowserActionTarget> {
        match self {
            Self::Click { target }
            | Self::Hover { target }
            | Self::Focus { target }
            | Self::Type { target, .. }
            | Self::Clear { target }
            | Self::Select { target, .. } => Some(target),
            Self::Keypress { target, .. } | Self::Scroll { target, .. } => target.as_ref(),
            Self::DragDrop { source, .. } => Some(source),
        }
    }

    pub fn is_mutating(&self) -> bool {
        !matches!(self, Self::Hover { .. } | Self::Focus { .. })
    }

    pub fn redacted_summary(&self) -> String {
        match self {
            Self::Click { target } => format!("click {}", target.diagnostic_name()),
            Self::Hover { target } => format!("hover {}", target.diagnostic_name()),
            Self::Focus { target } => format!("focus {}", target.diagnostic_name()),
            Self::Type { target, .. } => format!("type into {}", target.diagnostic_name()),
            Self::Clear { target } => format!("clear {}", target.diagnostic_name()),
            Self::Select { target, .. } => format!("select option in {}", target.diagnostic_name()),
            Self::Keypress { .. } => "keypress".to_string(),
            Self::Scroll { .. } => "scroll".to_string(),
            Self::DragDrop {
                source,
                destination,
            } => format!(
                "drag {} to {}",
                source.diagnostic_name(),
                destination.diagnostic_name()
            ),
        }
    }

    pub fn redacted_for_diagnostics(&self) -> BrowserRedactedAction {
        BrowserRedactedAction {
            summary: self.redacted_summary(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRedactedAction {
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct BrowserRawSemanticElement {
    pub role: Option<String>,
    pub name: Option<String>,
    pub label: Option<String>,
    pub text: Option<String>,
    pub test_id: Option<String>,
    pub css_selectors: Vec<String>,
    pub bounds: BrowserBounds,
    pub enabled: bool,
    pub checked: Option<bool>,
    pub value: Option<String>,
    pub input_type: Option<String>,
    pub interactive: bool,
}

impl Default for BrowserRawSemanticElement {
    fn default() -> Self {
        Self {
            role: None,
            name: None,
            label: None,
            text: None,
            test_id: None,
            css_selectors: Vec::new(),
            bounds: BrowserBounds {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            enabled: true,
            checked: None,
            value: None,
            input_type: None,
            interactive: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSemanticElement {
    pub element_ref: BrowserElementRef,
    pub role: Option<String>,
    pub name: Option<String>,
    pub label: Option<String>,
    pub text: Option<String>,
    pub bounds: BrowserBounds,
    pub enabled: bool,
    pub checked: Option<bool>,
    pub value: Option<String>,
    pub interactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSemanticSnapshot {
    pub revision: BrowserRevision,
    pub url: String,
    pub title: String,
    pub elements: Vec<BrowserSemanticElement>,
}

pub fn build_semantic_snapshot(
    revision: BrowserRevision,
    url: impl Into<String>,
    title: impl Into<String>,
    elements: Vec<BrowserRawSemanticElement>,
) -> BrowserSemanticSnapshot {
    let elements = elements
        .into_iter()
        .take(2_000)
        .map(|raw| {
            let locator = BrowserLocator {
                accessibility_role: clean_optional(raw.role.clone()),
                accessibility_name: clean_optional(raw.name.clone()),
                test_id: clean_optional(raw.test_id.clone()),
                css_selectors: raw
                    .css_selectors
                    .into_iter()
                    .filter(|value| !value.trim().is_empty())
                    .take(4)
                    .map(|value| truncate(value.trim(), 512))
                    .collect(),
            };
            let is_password = raw
                .input_type
                .as_deref()
                .is_some_and(|input_type| input_type.eq_ignore_ascii_case("password"));
            BrowserSemanticElement {
                element_ref: BrowserElementRef {
                    revision,
                    locator,
                    backend_node_id: None,
                },
                role: clean_optional(raw.role),
                name: clean_optional(raw.name),
                label: clean_optional(raw.label),
                text: raw.text.map(|value| truncate(value.trim(), 2_000)),
                bounds: raw.bounds,
                enabled: raw.enabled,
                checked: raw.checked,
                value: raw.value.map(|value| {
                    if is_password {
                        REDACTED_VALUE.to_string()
                    } else {
                        truncate(&value, 2_000)
                    }
                }),
                interactive: raw.interactive,
            }
        })
        .collect();
    BrowserSemanticSnapshot {
        revision,
        url: url.into(),
        title: title.into(),
        elements,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct BrowserRuntimeTarget {
    pub origin_url: String,
    pub role: Option<String>,
    pub name: Option<String>,
    pub input_type: Option<String>,
    pub form_action: Option<String>,
    pub permission: Option<String>,
}

pub fn effective_browser_risk(
    declared: BrowserRisk,
    runtime: Option<&BrowserRuntimeTarget>,
    path_risk: Option<BrowserRisk>,
) -> BrowserRisk {
    let runtime_risk = runtime
        .map(runtime_target_risk)
        .unwrap_or(BrowserRisk::Normal);
    [
        declared,
        runtime_risk,
        path_risk.unwrap_or(BrowserRisk::Normal),
    ]
    .into_iter()
    .max_by_key(|risk| risk_severity(*risk))
    .unwrap_or(BrowserRisk::Normal)
}

pub fn runtime_target_risk(target: &BrowserRuntimeTarget) -> BrowserRisk {
    if target
        .permission
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return BrowserRisk::PermissionChange;
    }
    let combined = format!(
        "{} {} {} {}",
        target.role.as_deref().unwrap_or_default(),
        target.name.as_deref().unwrap_or_default(),
        target.input_type.as_deref().unwrap_or_default(),
        target.form_action.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    if contains_any(
        &combined,
        &[
            "delete",
            "remove account",
            "erase",
            "destroy",
            "permanently",
        ],
    ) {
        BrowserRisk::Destructive
    } else if contains_any(
        &combined,
        &[
            "password", "security", "sign in", "login", "account", "2fa", "mfa",
        ],
    ) {
        BrowserRisk::AccountSecurity
    } else if contains_any(
        &combined,
        &["permission", "allow access", "grant access", "authorize"],
    ) {
        BrowserRisk::PermissionChange
    } else if contains_any(
        &combined,
        &[
            "purchase",
            "buy",
            "checkout",
            "pay",
            "credit card",
            "bank",
            "transfer",
        ],
    ) {
        BrowserRisk::Financial
    } else {
        BrowserRisk::Normal
    }
}

fn risk_severity(risk: BrowserRisk) -> u8 {
    match risk {
        BrowserRisk::Normal => 0,
        BrowserRisk::Financial => 1,
        BrowserRisk::Destructive => 2,
        BrowserRisk::AccountSecurity => 3,
        BrowserRisk::PermissionChange => 4,
        BrowserRisk::OutsideWorkspaceFile => 5,
        BrowserRisk::OsPermission => 6,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserTelemetryBuffer<T> {
    capacity: usize,
    values: VecDeque<T>,
}

impl<T> BrowserTelemetryBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            values: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, value: T) {
        if self.capacity == 0 {
            return;
        }
        while self.values.len() >= self.capacity {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    pub fn as_slice(&self) -> std::collections::vec_deque::Iter<'_, T> {
        self.values.iter()
    }

    pub fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.values.iter().cloned().collect()
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| truncate(&value, 2_000))
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

impl BrowserWorkspaceSnapshot {
    pub fn append_journal_entry(&mut self, mut entry: super::BrowserJournalEntry) {
        entry.intent = truncate(entry.intent.trim(), 512);
        entry.url = truncate(entry.url.trim(), 2_000);
        entry.result = truncate(entry.result.trim(), 128);
        while self.journal_entries.len() >= MAX_BROWSER_JOURNAL_ENTRIES {
            self.journal_entries.remove(0);
        }
        self.journal_entries.push(entry);
    }
}
