use super::{
    BrowserBounds, BrowserElementRef, BrowserError, BrowserLocator, BrowserRecipeLocator,
    BrowserRevision, BrowserRisk, BrowserWorkspaceSnapshot,
};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::LazyLock;

static SECRET_ASSIGNMENT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)(((?:[a-z0-9_-]*(?:token|secret|cookie)|(?:authorization|password|passwd)[a-z0-9_-]*|(?:api|private)[_-]?key))\s*[:=]\s*)([^\s,;&#]+)",
    )
    .expect("browser secret-assignment regex is valid")
});
static JSON_QUOTED_ASSIGNMENT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(\"((?:\\.|[^\"\\])*)\"\s*:\s*\")((?:\\.|[^\"\\])*)(\")"#)
        .expect("browser JSON quoted-assignment regex is valid")
});
static BEARER_SECRET: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+")
        .expect("browser bearer-token regex is valid")
});
static BASIC_SECRET: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\bBasic\s+[A-Za-z0-9._~+/=-]+")
        .expect("browser basic-credential regex is valid")
});
static BARE_CREDENTIAL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?ix)
        (?:^|[^A-Za-z0-9_-])eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}(?:$|[^A-Za-z0-9_-])
        |(?:^|[^A-Za-z0-9_-])sk-(?:proj-)?[A-Za-z0-9_-]{20,}(?:$|[^A-Za-z0-9_-])
        |(?:^|[^A-Za-z0-9_-])gh[pousr]_[A-Za-z0-9]{20,}(?:$|[^A-Za-z0-9])
        |(?:^|[^A-Z0-9])(?:AKIA|ASIA)[A-Z0-9]{16}(?:$|[^A-Z0-9])
        |(?:^|[^A-Za-z0-9_-])AIza[A-Za-z0-9_-]{30,}(?:$|[^A-Za-z0-9_-])",
    )
    .expect("browser bare-credential regex is valid")
});

pub const MAX_BROWSER_ACTIONS: usize = 32;
pub const MAX_BROWSER_JOURNAL_ENTRIES: usize = 100;
pub const REDACTED_VALUE: &str = "[redacted]";

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserReplayRepairCandidate {
    element_ref: BrowserElementRef,
}

impl BrowserReplayRepairCandidate {
    pub fn new(element_ref: BrowserElementRef) -> Self {
        Self { element_ref }
    }

    pub fn element_ref(&self) -> &BrowserElementRef {
        &self.element_ref
    }

    pub(crate) fn validated_recipe_locator(&self) -> Result<BrowserRecipeLocator, BrowserError> {
        let locator = BrowserRecipeLocator::from(self.element_ref.locator.clone());
        let encoded = serde_json::to_value(locator).map_err(|_| BrowserError::InvalidRecipe {
            message: "repair candidate locator is invalid".to_string(),
        })?;
        serde_json::from_value(encoded).map_err(|_| BrowserError::InvalidRecipe {
            message: "repair candidate locator is invalid".to_string(),
        })
    }

    pub(crate) fn action_target(&self) -> BrowserActionTarget {
        BrowserActionTarget::from_element_ref(self.element_ref.clone())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq, Default,
)]
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

#[derive(Debug, Clone, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserScreenshotMode {
    Viewport,
    FullPage,
}

#[derive(Debug, Clone, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserWaitCondition {
    Duration {
        duration_ms: u64,
    },
    Url {
        value: String,
        exact: bool,
    },
    Load,
    NetworkIdle,
    Title {
        value: String,
        exact: bool,
    },
    ElementPresent {
        target: BrowserActionTarget,
    },
    ElementAbsent {
        target: BrowserActionTarget,
    },
    ElementVisible {
        target: BrowserActionTarget,
    },
    ElementHidden {
        target: BrowserActionTarget,
    },
    ElementValue {
        target: BrowserActionTarget,
        value: String,
    },
    TextPresent {
        text: String,
    },
    TextAbsent {
        text: String,
    },
    JavaScript {
        predicate: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserConsoleOperation {
    List,
    Clear,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserNetworkOperation {
    List,
    Clear,
    Body,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserPerformanceOperation {
    Snapshot,
    TraceStart,
    TraceStop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
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
            let is_password = raw
                .input_type
                .as_deref()
                .is_some_and(|input_type| input_type.eq_ignore_ascii_case("password"));
            let password_value = raw.value.as_deref();
            let name = clean_semantic_metadata(raw.name, is_password, password_value, 2_000);
            let label = clean_semantic_metadata(raw.label, is_password, password_value, 2_000);
            let text = clean_semantic_metadata(raw.text, is_password, password_value, 2_000);
            let locator = BrowserLocator {
                accessibility_role: clean_optional(raw.role.clone()),
                accessibility_name: name.clone(),
                test_id: clean_optional(raw.test_id.clone()),
                css_selectors: raw
                    .css_selectors
                    .into_iter()
                    .filter(|value| !value.trim().is_empty())
                    .take(4)
                    .map(|value| truncate(value.trim(), 512))
                    .collect(),
            };
            BrowserSemanticElement {
                element_ref: BrowserElementRef {
                    revision,
                    locator,
                    backend_node_id: None,
                },
                role: clean_optional(raw.role),
                name,
                label,
                text,
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
    pub autocomplete: Option<String>,
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

pub fn effective_browser_risk_for_targets(
    declared: BrowserRisk,
    runtime_targets: &[BrowserRuntimeTarget],
    path_risk: Option<BrowserRisk>,
) -> BrowserRisk {
    runtime_targets.iter().fold(
        effective_browser_risk(declared, None, path_risk),
        |risk, runtime| effective_browser_risk(risk, Some(runtime), None),
    )
}

pub fn effective_browser_secret_type_risk(
    declared: BrowserRisk,
    runtime_target: &BrowserRuntimeTarget,
) -> BrowserRisk {
    effective_browser_risk(
        declared,
        Some(runtime_target),
        Some(BrowserRisk::AccountSecurity),
    )
}

pub fn browser_cdp_method_risk(method: &str) -> BrowserRisk {
    match method {
        "Browser.getVersion"
        | "DOM.describeNode"
        | "DOM.getAttributes"
        | "DOM.getBoxModel"
        | "DOM.getDocument"
        | "DOM.getNodeForLocation"
        | "DOM.querySelector"
        | "DOM.querySelectorAll"
        | "DOM.enable"
        | "Network.enable"
        | "Page.enable"
        | "Page.getFrameTree"
        | "Page.getLayoutMetrics"
        | "Page.getNavigationHistory"
        | "Performance.enable"
        | "Performance.getMetrics"
        | "Runtime.enable"
        | "Runtime.getIsolateId" => BrowserRisk::Normal,
        _ => BrowserRisk::Destructive,
    }
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
        "{} {} {} {} {}",
        target.role.as_deref().unwrap_or_default(),
        target.name.as_deref().unwrap_or_default(),
        target.input_type.as_deref().unwrap_or_default(),
        target.autocomplete.as_deref().unwrap_or_default(),
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

fn clean_semantic_metadata(
    value: Option<String>,
    is_password: bool,
    password_value: Option<&str>,
    max_chars: usize,
) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .filter(|value| {
            !is_password
                || !password_value
                    .map(str::trim)
                    .filter(|secret| !secret.is_empty())
                    .is_some_and(|secret| value == secret)
        })
        .map(|value| truncate(&value, max_chars))
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

pub fn redact_browser_text(value: &str) -> String {
    let redacted = serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|mut value| {
            redact_json_value(&mut value);
            serde_json::to_string(&value).ok()
        })
        .unwrap_or_else(|| redact_browser_secrets(value));
    redacted.chars().take(4_000).collect()
}

pub(crate) fn browser_text_contains_secret(value: &str) -> bool {
    if BARE_CREDENTIAL.is_match(value) {
        return true;
    }
    if let Ok(mut redacted) = serde_json::from_str::<Value>(value) {
        let original = redacted.clone();
        redact_json_value(&mut redacted);
        return redacted != original;
    }
    redact_browser_secrets(value) != value
}

fn redact_browser_secrets(value: &str) -> String {
    let redacted = BASIC_SECRET.replace_all(value, format!("Basic {REDACTED_VALUE}"));
    let redacted = BEARER_SECRET.replace_all(&redacted, format!("Bearer {REDACTED_VALUE}"));
    let redacted =
        JSON_QUOTED_ASSIGNMENT.replace_all(&redacted, |captures: &regex::Captures<'_>| {
            if browser_secret_key(&captures[2]) {
                format!("{}{REDACTED_VALUE}{}", &captures[1], &captures[4])
            } else {
                captures[0].to_string()
            }
        });
    SECRET_ASSIGNMENT
        .replace_all(&redacted, format!("$1{REDACTED_VALUE}"))
        .into_owned()
}

pub fn redact_browser_resource_bytes(mime_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mime_type = mime_type.to_ascii_lowercase();
    let text_like = mime_type.contains("json")
        || mime_type.starts_with("text/")
        || mime_type.contains("javascript")
        || mime_type.contains("xml")
        || mime_type.contains("form");
    if !text_like {
        return bytes.to_vec();
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    if mime_type.contains("json") {
        if let Ok(mut value) = serde_json::from_str::<Value>(text) {
            redact_json_value(&mut value);
            if let Ok(encoded) = serde_json::to_vec(&value) {
                return encoded;
            }
        }
    }
    redact_browser_secrets(text).into_bytes()
}

fn redact_json_value(value: &mut Value) {
    match value {
        Value::Object(entries) => {
            for (key, value) in entries {
                if browser_secret_key(key) {
                    *value = Value::String(REDACTED_VALUE.to_string());
                } else {
                    redact_json_value(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        Value::String(value) => *value = redact_browser_secrets(value),
        _ => {}
    }
}

fn browser_secret_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    matches!(normalized.as_str(), "apikey" | "privatekey")
        || ["token", "secret", "cookie"]
            .iter()
            .any(|suffix| normalized == *suffix || normalized.ends_with(suffix))
        || ["authorization", "password", "passwd"]
            .iter()
            .any(|prefix| normalized == *prefix || normalized.starts_with(prefix))
}

impl BrowserWorkspaceSnapshot {
    pub fn append_journal_entry(&mut self, mut entry: super::BrowserJournalEntry) {
        entry.intent = truncate(&redact_browser_text(entry.intent.trim()), 512);
        entry.url = truncate(&redact_browser_text(entry.url.trim()), 2_000);
        entry.result = truncate(&redact_browser_text(entry.result.trim()), 128);
        while self.journal_entries.len() >= MAX_BROWSER_JOURNAL_ENTRIES {
            self.journal_entries.remove(0);
        }
        self.journal_entries.push(entry);
    }
}
